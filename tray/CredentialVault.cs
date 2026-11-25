using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using Windows.Security.Credentials.UI;

namespace FocusStageTray;

internal enum HelloVerifyStatus
{
    Verified,
    Canceled,
    Unavailable,
    Error,
}

internal readonly record struct HelloStatusInfo(
    bool Available,
    string Code,
    string Message);

internal readonly record struct HelloVerifyResult(
    HelloVerifyStatus Status,
    string? Message);

internal readonly record struct CredentialMetadata(
    bool HasLinuxPassword,
    bool HasKeyringPassword,
    bool KeyringUsesLinuxPassword,
    string? UpdatedAtUtc);

internal static class CredentialVault
{
    private const int CryptProtectUiForbidden = 0x1;
    private static readonly object FileLock = new();

    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        WriteIndented = true,
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
    };

    [StructLayout(LayoutKind.Sequential)]
    private struct DataBlob
    {
        public int cbData;
        public IntPtr pbData;
    }

    [DllImport("crypt32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CryptProtectData(
        ref DataBlob pDataIn,
        string? szDataDescr,
        ref DataBlob pOptionalEntropy,
        IntPtr pvReserved,
        IntPtr pPromptStruct,
        int dwFlags,
        out DataBlob pDataOut);

    [DllImport("crypt32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CryptUnprotectData(
        ref DataBlob pDataIn,
        IntPtr ppszDataDescr,
        ref DataBlob pOptionalEntropy,
        IntPtr pvReserved,
        IntPtr pPromptStruct,
        int dwFlags,
        out DataBlob pDataOut);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LocalFree(IntPtr hMem);

    private sealed class VaultDocument
    {
        [JsonPropertyName("version")]
        public int Version { get; set; } = 1;

        [JsonPropertyName("autoHelloForKeyring")]
        public bool AutoHelloForKeyring { get; set; } = true;

        [JsonPropertyName("entries")]
        public Dictionary<string, VaultEntry> Entries { get; set; } = new(StringComparer.Ordinal);
    }

    private sealed class VaultEntry
    {
        [JsonPropertyName("distro")]
        public string Distro { get; set; } = "Ubuntu";

        [JsonPropertyName("username")]
        public string Username { get; set; } = "";

        [JsonPropertyName("linuxCipher")]
        public string? LinuxCipher { get; set; }

        [JsonPropertyName("keyringCipher")]
        public string? KeyringCipher { get; set; }

        [JsonPropertyName("keyringUsesLinuxPassword")]
        public bool KeyringUsesLinuxPassword { get; set; } = true;

        [JsonPropertyName("updatedAtUtc")]
        public string? UpdatedAtUtc { get; set; }
    }

    private static string VaultPath => Path.Combine(ShortcutManager.AppInstallDir, "credentials.json");

    private static string MakeKey(string distro, string username)
    {
        var d = string.IsNullOrWhiteSpace(distro) ? "Ubuntu" : distro.Trim();
        var u = username.Trim();
        return $"{d}\n{u}";
    }

    private static VaultDocument LoadDocument()
    {
        lock (FileLock)
        {
            try
            {
                if (!File.Exists(VaultPath)) return new VaultDocument();
                var json = File.ReadAllText(VaultPath, Encoding.UTF8);
                return JsonSerializer.Deserialize<VaultDocument>(json, JsonOptions) ?? new VaultDocument();
            }
            catch
            {
                return new VaultDocument();
            }
        }
    }

    private static void SaveDocument(VaultDocument doc)
    {
        lock (FileLock)
        {
            Directory.CreateDirectory(ShortcutManager.AppInstallDir);
            var tmp = VaultPath + ".tmp";
            var json = JsonSerializer.Serialize(doc, JsonOptions);
            File.WriteAllText(tmp, json, new UTF8Encoding(encoderShouldEmitUTF8Identifier: false));
            File.Move(tmp, VaultPath, overwrite: true);
        }
    }

    public static bool IsAutoHelloForKeyringEnabled() => LoadDocument().AutoHelloForKeyring;

    public static void SetAutoHelloForKeyring(bool enabled)
    {
        lock (FileLock)
        {
            var doc = LoadDocument();
            doc.AutoHelloForKeyring = enabled;
            SaveDocument(doc);
        }
    }

    public static CredentialMetadata GetMetadata(string distro, string username)
    {
        if (string.IsNullOrWhiteSpace(username))
        {
            return new CredentialMetadata(false, false, true, null);
        }
        var doc = LoadDocument();
        if (!doc.Entries.TryGetValue(MakeKey(distro, username), out var entry))
        {
            return new CredentialMetadata(false, false, true, null);
        }
        bool hasLinux = !string.IsNullOrEmpty(entry.LinuxCipher);
        bool hasKeyring = entry.KeyringUsesLinuxPassword
            ? hasLinux
            : !string.IsNullOrEmpty(entry.KeyringCipher);
        return new CredentialMetadata(hasLinux, hasKeyring, entry.KeyringUsesLinuxPassword, entry.UpdatedAtUtc);
    }

    public static bool HasSecret(string distro, string username, bool forKeyring)
    {
        var meta = GetMetadata(distro, username);
        return forKeyring ? meta.HasKeyringPassword : meta.HasLinuxPassword;
    }

    public static async Task<HelloStatusInfo> GetHelloStatusAsync(CancellationToken cancellation = default)
    {
        try
        {
            var availability = await UserConsentVerifier.CheckAvailabilityAsync().AsTask(cancellation);
            return availability switch
            {
                UserConsentVerifierAvailability.Available => new HelloStatusInfo(
                    true,
                    "available",
                    "Windows Hello 已就绪（可使用 PIN / 指纹 / 人脸保护密码）"),
                UserConsentVerifierAvailability.DeviceNotPresent => new HelloStatusInfo(
                    false,
                    "device_not_present",
                    "当前设备未启用 Windows Hello 安全模块。"),
                UserConsentVerifierAvailability.NotConfiguredForUser => new HelloStatusInfo(
                    false,
                    "not_configured",
                    "尚未设置 Windows Hello，请先在 Windows 设置 → 账户 → 登录选项 中配置 PIN 或生物识别。"),
                UserConsentVerifierAvailability.DisabledByPolicy => new HelloStatusInfo(
                    false,
                    "disabled_by_policy",
                    "Windows Hello 已被系统策略禁用。"),
                UserConsentVerifierAvailability.DeviceBusy => new HelloStatusInfo(
                    false,
                    "device_busy",
                    "Windows Hello 设备正忙，请稍后再试。"),
                _ => new HelloStatusInfo(false, "unknown", "无法检测 Windows Hello 状态。"),
            };
        }
        catch (Exception ex)
        {
            return new HelloStatusInfo(false, "error", $"无法检测 Windows Hello: {ex.Message}");
        }
    }

    public static async Task<HelloVerifyResult> VerifyHelloAsync(
        nint hwnd,
        string reason,
        CancellationToken cancellation = default)
    {
        var status = await GetHelloStatusAsync(cancellation);
        if (!status.Available)
        {
            return new HelloVerifyResult(HelloVerifyStatus.Unavailable, status.Message);
        }

        try
        {
            var result = hwnd != IntPtr.Zero
                ? await UserConsentVerifierInterop.RequestVerificationForWindowAsync(hwnd, reason).AsTask(cancellation)
                : await UserConsentVerifier.RequestVerificationAsync(reason).AsTask(cancellation);

            return result switch
            {
                UserConsentVerificationResult.Verified => new HelloVerifyResult(HelloVerifyStatus.Verified, null),
                UserConsentVerificationResult.Canceled => new HelloVerifyResult(HelloVerifyStatus.Canceled, "已取消 Windows Hello 验证。"),
                UserConsentVerificationResult.RetriesExhausted => new HelloVerifyResult(HelloVerifyStatus.Canceled, "Windows Hello 验证重试次数过多。"),
                UserConsentVerificationResult.DeviceBusy => new HelloVerifyResult(HelloVerifyStatus.Error, "Windows Hello 设备正忙，请稍后重试。"),
                UserConsentVerificationResult.DeviceNotPresent or UserConsentVerificationResult.NotConfiguredForUser
                    => new HelloVerifyResult(HelloVerifyStatus.Unavailable, "尚未配置 Windows Hello（PIN / 生物识别）。"),
                _ => new HelloVerifyResult(HelloVerifyStatus.Error, "Windows Hello 验证未通过。"),
            };
        }
        catch (OperationCanceledException)
        {
            return new HelloVerifyResult(HelloVerifyStatus.Canceled, "已取消 Windows Hello 验证。");
        }
        catch (Exception ex)
        {
            return new HelloVerifyResult(HelloVerifyStatus.Error, $"Windows Hello 调用失败: {ex.Message}");
        }
    }

    public static async Task<HelloVerifyResult> SaveCredentialWithHelloAsync(
        nint hwnd,
        string distro,
        string username,
        string? linuxPassword,
        bool updateLinux,
        bool keyringUsesLinuxPassword,
        string? keyringPassword,
        bool updateKeyring,
        bool requireHello = true,
        CancellationToken cancellation = default)
    {
        if (string.IsNullOrWhiteSpace(username))
        {
            return new HelloVerifyResult(HelloVerifyStatus.Error, "用户名不能为空。");
        }

        if (requireHello)
        {
            var verify = await VerifyHelloAsync(
                hwnd,
                $"验证身份以使用 Windows Hello 托管 Linux 账号 “{username}” 的密码",
                cancellation);
            if (verify.Status != HelloVerifyStatus.Verified)
            {
                return verify;
            }
        }

        SaveCredentialInternal(
            distro,
            username,
            linuxPassword,
            updateLinux,
            keyringUsesLinuxPassword,
            keyringPassword,
            updateKeyring);
        return new HelloVerifyResult(HelloVerifyStatus.Verified, null);
    }

    public static void SaveCredentialAfterVerification(
        string distro,
        string username,
        string? linuxPassword,
        bool updateLinux,
        bool keyringUsesLinuxPassword,
        string? keyringPassword,
        bool updateKeyring)
    {
        SaveCredentialInternal(
            distro,
            username,
            linuxPassword,
            updateLinux,
            keyringUsesLinuxPassword,
            keyringPassword,
            updateKeyring);
    }

    private static void SaveCredentialInternal(
        string distro,
        string username,
        string? linuxPassword,
        bool updateLinux,
        bool keyringUsesLinuxPassword,
        string? keyringPassword,
        bool updateKeyring)
    {
        lock (FileLock)
        {
            var doc = LoadDocument();
            var key = MakeKey(distro, username);
            if (!doc.Entries.TryGetValue(key, out var entry))
            {
                entry = new VaultEntry
                {
                    Distro = string.IsNullOrWhiteSpace(distro) ? "Ubuntu" : distro.Trim(),
                    Username = username.Trim(),
                };
            }

            if (updateLinux)
            {
                entry.LinuxCipher = string.IsNullOrEmpty(linuxPassword)
                    ? null
                    : ProtectSecret(linuxPassword, BuildEntropy("linux", distro, username));
            }

            entry.KeyringUsesLinuxPassword = keyringUsesLinuxPassword;
            if (keyringUsesLinuxPassword)
            {
                entry.KeyringCipher = null;
            }
            else if (updateKeyring)
            {
                entry.KeyringCipher = string.IsNullOrEmpty(keyringPassword)
                    ? null
                    : ProtectSecret(keyringPassword, BuildEntropy("keyring", distro, username));
            }

            entry.UpdatedAtUtc = DateTime.UtcNow.ToString("O");
            if (string.IsNullOrEmpty(entry.LinuxCipher) && string.IsNullOrEmpty(entry.KeyringCipher))
            {
                doc.Entries.Remove(key);
            }
            else
            {
                doc.Entries[key] = entry;
            }
            SaveDocument(doc);
        }
    }

    public static async Task<(HelloVerifyResult Verify, string? Secret)> RetrieveSecretWithHelloAsync(
        nint hwnd,
        string distro,
        string username,
        bool forKeyring,
        string reason,
        CancellationToken cancellation = default)
    {
        string? cipher;
        string entropyKind;
        lock (FileLock)
        {
            var doc = LoadDocument();
            if (!doc.Entries.TryGetValue(MakeKey(distro, username), out var entry))
            {
                return (new HelloVerifyResult(HelloVerifyStatus.Error, $"尚未为 “{username}” 保存托管凭据。"), null);
            }

            if (forKeyring)
            {
                if (entry.KeyringUsesLinuxPassword)
                {
                    cipher = entry.LinuxCipher;
                    entropyKind = "linux";
                }
                else
                {
                    cipher = entry.KeyringCipher;
                    entropyKind = "keyring";
                }
            }
            else
            {
                cipher = entry.LinuxCipher;
                entropyKind = "linux";
            }
        }

        if (string.IsNullOrEmpty(cipher))
        {
            return (new HelloVerifyResult(HelloVerifyStatus.Error, forKeyring
                ? $"尚未为 “{username}” 保存密钥环密码。"
                : $"尚未为 “{username}” 保存 Linux 密码。"), null);
        }

        var verify = await VerifyHelloAsync(hwnd, reason, cancellation);
        if (verify.Status != HelloVerifyStatus.Verified)
        {
            return (verify, null);
        }

        try
        {
            var secret = UnprotectSecret(cipher, BuildEntropy(entropyKind, distro, username));
            return (verify, secret);
        }
        catch (Exception ex)
        {
            return (new HelloVerifyResult(HelloVerifyStatus.Error, $"解密已存凭据失败（可能需要重新绑定）: {ex.Message}"), null);
        }
    }

    public static void DeleteCredential(string distro, string username, bool clearLinux = true, bool clearKeyring = true)
    {
        lock (FileLock)
        {
            var doc = LoadDocument();
            var key = MakeKey(distro, username);
            if (!doc.Entries.TryGetValue(key, out var entry)) return;

            if (clearLinux) entry.LinuxCipher = null;
            if (clearKeyring)
            {
                entry.KeyringCipher = null;
                if (clearLinux) entry.KeyringUsesLinuxPassword = true;
            }

            if (string.IsNullOrEmpty(entry.LinuxCipher) && string.IsNullOrEmpty(entry.KeyringCipher))
            {
                doc.Entries.Remove(key);
            }
            else
            {
                entry.UpdatedAtUtc = DateTime.UtcNow.ToString("O");
                doc.Entries[key] = entry;
            }
            SaveDocument(doc);
        }
    }

    private static byte[] BuildEntropy(string kind, string distro, string username)
    {
        var d = string.IsNullOrWhiteSpace(distro) ? "Ubuntu" : distro.Trim();
        return Encoding.UTF8.GetBytes($"FocusStage.CredentialVault.v1|{kind}|{d}|{username.Trim()}");
    }

    private static string ProtectSecret(string secret, byte[] entropy)
    {
        var plainBytes = Encoding.UTF8.GetBytes(secret);
        var dataIn = new DataBlob();
        var entropyBlob = new DataBlob();
        var dataOut = new DataBlob();
        try
        {
            dataIn.cbData = plainBytes.Length;
            dataIn.pbData = Marshal.AllocHGlobal(plainBytes.Length);
            Marshal.Copy(plainBytes, 0, dataIn.pbData, plainBytes.Length);

            entropyBlob.cbData = entropy.Length;
            entropyBlob.pbData = Marshal.AllocHGlobal(entropy.Length);
            Marshal.Copy(entropy, 0, entropyBlob.pbData, entropy.Length);

            if (!CryptProtectData(
                    ref dataIn,
                    "FocusStage Linux Credential",
                    ref entropyBlob,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    CryptProtectUiForbidden,
                    out dataOut))
            {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            }

            var protectedBytes = new byte[dataOut.cbData];
            Marshal.Copy(dataOut.pbData, protectedBytes, 0, dataOut.cbData);
            return Convert.ToBase64String(protectedBytes);
        }
        finally
        {
            Array.Clear(plainBytes);
            if (dataIn.pbData != IntPtr.Zero)
            {
                CryptographicOperations.ZeroMemory(new Span<byte>(plainBytes));
                ZeroAndFreeHGlobal(dataIn.pbData, dataIn.cbData);
            }
            if (entropyBlob.pbData != IntPtr.Zero)
            {
                ZeroAndFreeHGlobal(entropyBlob.pbData, entropyBlob.cbData);
            }
            if (dataOut.pbData != IntPtr.Zero)
            {
                LocalFree(dataOut.pbData);
            }
        }
    }

    private static string UnprotectSecret(string base64Cipher, byte[] entropy)
    {
        var cipherBytes = Convert.FromBase64String(base64Cipher);
        var dataIn = new DataBlob();
        var entropyBlob = new DataBlob();
        var dataOut = new DataBlob();
        byte[]? plainBytes = null;
        try
        {
            dataIn.cbData = cipherBytes.Length;
            dataIn.pbData = Marshal.AllocHGlobal(cipherBytes.Length);
            Marshal.Copy(cipherBytes, 0, dataIn.pbData, cipherBytes.Length);

            entropyBlob.cbData = entropy.Length;
            entropyBlob.pbData = Marshal.AllocHGlobal(entropy.Length);
            Marshal.Copy(entropy, 0, entropyBlob.pbData, entropy.Length);

            if (!CryptUnprotectData(
                    ref dataIn,
                    IntPtr.Zero,
                    ref entropyBlob,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    CryptProtectUiForbidden,
                    out dataOut))
            {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            }

            plainBytes = new byte[dataOut.cbData];
            Marshal.Copy(dataOut.pbData, plainBytes, 0, dataOut.cbData);
            return Encoding.UTF8.GetString(plainBytes);
        }
        finally
        {
            if (plainBytes is not null)
            {
                Array.Clear(plainBytes);
            }
            if (dataIn.pbData != IntPtr.Zero)
            {
                ZeroAndFreeHGlobal(dataIn.pbData, dataIn.cbData);
            }
            if (entropyBlob.pbData != IntPtr.Zero)
            {
                ZeroAndFreeHGlobal(entropyBlob.pbData, entropyBlob.cbData);
            }
            if (dataOut.pbData != IntPtr.Zero)
            {
                ZeroUnmanaged(dataOut.pbData, dataOut.cbData);
                LocalFree(dataOut.pbData);
            }
        }
    }

    private static void ZeroUnmanaged(IntPtr ptr, int len)
    {
        if (ptr == IntPtr.Zero || len <= 0) return;
        Marshal.Copy(new byte[len], 0, ptr, len);
    }

    private static void ZeroAndFreeHGlobal(IntPtr ptr, int len)
    {
        ZeroUnmanaged(ptr, len);
        Marshal.FreeHGlobal(ptr);
    }
}
