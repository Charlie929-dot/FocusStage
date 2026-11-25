using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusStageTray;

internal static class AccountService
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNameCaseInsensitive = true,
    };

    private sealed class RawAccountsStatus
    {
        [JsonPropertyName("distro")]
        public string Distro { get; set; } = "Ubuntu";

        [JsonPropertyName("current_user")]
        public string CurrentUser { get; set; } = "";

        [JsonPropertyName("default_wsl_user")]
        public string? DefaultWslUser { get; set; }

        [JsonPropertyName("accounts")]
        public List<RawAccountInfo> Accounts { get; set; } = new();

        [JsonPropertyName("keyring")]
        public RawKeyringStatus Keyring { get; set; } = new();
    }

    private sealed class RawAccountInfo
    {
        [JsonPropertyName("username")]
        public string Username { get; set; } = "";

        [JsonPropertyName("uid")]
        public uint Uid { get; set; }

        [JsonPropertyName("gid")]
        public uint Gid { get; set; }

        [JsonPropertyName("full_name")]
        public string FullName { get; set; } = "";

        [JsonPropertyName("home")]
        public string Home { get; set; } = "";

        [JsonPropertyName("shell")]
        public string Shell { get; set; } = "";

        [JsonPropertyName("is_admin")]
        public bool IsAdmin { get; set; }

        [JsonPropertyName("is_current")]
        public bool IsCurrent { get; set; }

        [JsonPropertyName("is_default_wsl")]
        public bool IsDefaultWsl { get; set; }
    }

    private sealed class RawKeyringStatus
    {
        [JsonPropertyName("available")]
        public bool Available { get; set; }

        [JsonPropertyName("default_collection")]
        public string? DefaultCollection { get; set; }

        [JsonPropertyName("label")]
        public string? Label { get; set; }

        [JsonPropertyName("locked")]
        public bool? Locked { get; set; }
    }

    public static async Task<object> GetViewAsync(string distro)
    {
        var helloTask = CredentialVault.GetHelloStatusAsync();
        var wslTask = ShortcutManager.RunWslAsync(distro, false, 15, "focusstage", "--json", "accounts", "status");
        await Task.WhenAll(helloTask, wslTask);

        var hello = await helloTask;
        var (code, stdout, stderr) = await wslTask;
        if (code != 0 || string.IsNullOrWhiteSpace(stdout))
        {
            throw new InvalidOperationException(
                string.IsNullOrWhiteSpace(stderr) ? $"读取 WSL 账号信息失败 (exit {code})" : stderr);
        }

        var raw = JsonSerializer.Deserialize<RawAccountsStatus>(stdout, JsonOptions)
            ?? throw new InvalidOperationException("WSL 账号响应为空。");

        int adminCount = raw.Accounts.Count(a => a.Uid >= 1000 && a.IsAdmin);
        var effectiveDistro = string.IsNullOrWhiteSpace(raw.Distro) ? distro : raw.Distro;

        var accounts = raw.Accounts.Select(a =>
        {
            var meta = CredentialVault.GetMetadata(effectiveDistro, a.Username);
            return new
            {
                username = a.Username,
                uid = a.Uid,
                gid = a.Gid,
                fullName = a.FullName,
                home = a.Home,
                shell = a.Shell,
                isAdmin = a.IsAdmin,
                isLastAdmin = a.IsAdmin && adminCount <= 1,
                isCurrent = a.IsCurrent,
                isDefaultWsl = a.IsDefaultWsl,
                hasLinuxCredential = meta.HasLinuxPassword,
                hasKeyringCredential = meta.HasKeyringPassword,
                keyringUsesLinuxPassword = meta.KeyringUsesLinuxPassword,
                credentialUpdatedAtUtc = meta.UpdatedAtUtc,
            };
        }).ToList();

        return new
        {
            distro = effectiveDistro,
            currentUser = raw.CurrentUser,
            defaultWslUser = raw.DefaultWslUser,
            autoHelloForKeyring = CredentialVault.IsAutoHelloForKeyringEnabled(),
            hello = new
            {
                available = hello.Available,
                code = hello.Code,
                message = hello.Message,
            },
            keyring = new
            {
                available = raw.Keyring.Available,
                defaultCollection = raw.Keyring.DefaultCollection,
                label = raw.Keyring.Label,
                locked = raw.Keyring.Locked,
            },
            accounts,
        };
    }

    public static async Task<(bool Ok, string Message)> VerifyLinuxPasswordAsync(
        string distro,
        string username,
        string password)
    {
        var (code, stdout, stderr) = await ShortcutManager.RunWslWithStdinAsync(
            distro,
            false,
            15,
            password + "\n",
            "focusstage",
            "--json",
            "accounts",
            "verify-password",
            "--username",
            username);

        if (code != 0 || string.IsNullOrWhiteSpace(stdout))
        {
            var msg = ExtractJsonMessage(stdout);
            return (false, !string.IsNullOrWhiteSpace(msg)
                ? msg
                : (string.IsNullOrWhiteSpace(stderr) ? "验证 Linux 密码失败。" : stderr));
        }

        try
        {
            using var doc = JsonDocument.Parse(stdout);
            bool valid = doc.RootElement.TryGetProperty("valid", out var v) && v.GetBoolean();
            return valid
                ? (true, $"“{username}” 的 Linux 密码验证通过。")
                : (false, $"“{username}” 的 Linux 密码不正确。");
        }
        catch
        {
            return (false, "无法解析密码验证结果。");
        }
    }

    public static async Task<object> BindCredentialAsync(
        nint hwnd,
        string distro,
        string username,
        string? linuxPassword,
        bool updateLinux,
        bool keyringUsesLinuxPassword,
        string? keyringPassword,
        bool updateKeyring,
        bool verifyLinuxBeforeSave)
    {
        if (string.IsNullOrWhiteSpace(username))
        {
            return new { ok = false, message = "请指定账号。" };
        }

        if (verifyLinuxBeforeSave && updateLinux && !string.IsNullOrEmpty(linuxPassword))
        {
            var (valid, verifyMsg) = await VerifyLinuxPasswordAsync(distro, username, linuxPassword);
            if (!valid)
            {
                return new { ok = false, message = verifyMsg };
            }
        }

        var saveRes = await CredentialVault.SaveCredentialWithHelloAsync(
            hwnd,
            distro,
            username,
            linuxPassword,
            updateLinux,
            keyringUsesLinuxPassword,
            keyringPassword,
            updateKeyring,
            requireHello: true);

        if (saveRes.Status != HelloVerifyStatus.Verified)
        {
            return new { ok = false, message = saveRes.Message ?? "Windows Hello 验证未通过，未能保存。" };
        }

        var view = await GetViewAsync(distro);
        return new
        {
            ok = true,
            message = $"已通过 Windows Hello 托管 “{username}” 的密码凭据。",
            view,
        };
    }

    public static async Task<object> TestStoredCredentialAsync(nint hwnd, string distro, string username)
    {
        var (verify, secret) = await CredentialVault.RetrieveSecretWithHelloAsync(
            hwnd,
            distro,
            username,
            forKeyring: false,
            $"验证 Windows Hello 以测试 “{username}” 的 Linux 密码");

        if (verify.Status != HelloVerifyStatus.Verified || secret is null)
        {
            return new { ok = false, message = verify.Message ?? "Windows Hello 验证未通过。" };
        }

        var (valid, msg) = await VerifyLinuxPasswordAsync(distro, username, secret);
        return new { ok = valid, message = msg };
    }

    public static async Task<object> ClearCredentialAsync(string distro, string username)
    {
        CredentialVault.DeleteCredential(distro, username, clearLinux: true, clearKeyring: true);
        var view = await GetViewAsync(distro);
        return new
        {
            ok = true,
            message = $"已清除 “{username}” 的 Windows Hello 托管凭据。",
            view,
        };
    }

    public static async Task<object> UnlockKeyringWithHelloAsync(nint hwnd, string distro, string username)
    {
        var (verify, secret) = await CredentialVault.RetrieveSecretWithHelloAsync(
            hwnd,
            distro,
            username,
            forKeyring: true,
            $"验证 Windows Hello 以解锁 “{username}” 的 GNOME 密钥环");

        if (verify.Status != HelloVerifyStatus.Verified || secret is null)
        {
            return new { ok = false, message = verify.Message ?? "Windows Hello 验证未通过。" };
        }

        return await UnlockKeyringWithPasswordAsync(distro, secret);
    }

    public static async Task<object> UnlockKeyringWithPasswordAsync(string distro, string password)
    {
        var (code, stdout, stderr) = await ShortcutManager.RunWslWithStdinAsync(
            distro,
            false,
            15,
            password + "\n",
            "focusstage",
            "--json",
            "keyring",
            "unlock");

        var msg = ExtractJsonMessage(stdout);
        var view = await GetViewAsync(distro);
        if (code == 0)
        {
            return new { ok = true, message = "GNOME 密钥环已解锁。", view };
        }
        return new
        {
            ok = false,
            message = !string.IsNullOrWhiteSpace(msg)
                ? msg
                : (string.IsNullOrWhiteSpace(stderr) ? "解锁密钥环失败。" : stderr),
            view,
        };
    }

    public static async Task<object> LockKeyringAsync(string distro)
    {
        var (code, stdout, stderr) = await ShortcutManager.RunWslAsync(
            distro,
            false,
            15,
            "focusstage",
            "--json",
            "keyring",
            "lock");

        var msg = ExtractJsonMessage(stdout);
        var view = await GetViewAsync(distro);
        if (code == 0)
        {
            return new { ok = true, message = "GNOME 密钥环已锁定。", view };
        }
        return new
        {
            ok = false,
            message = !string.IsNullOrWhiteSpace(msg)
                ? msg
                : (string.IsNullOrWhiteSpace(stderr) ? "锁定密钥环失败。" : stderr),
            view,
        };
    }

    public static async Task<object> ApplyAccountActionAsync(
        FocusStageTrayContext ctx,
        nint hwnd,
        string distro,
        string actionJson,
        bool saveToHello,
        string? usernameToSync,
        string? passwordToSync,
        bool isAdminForCreate = false)
    {
        if (!ctx.DaemonAlive)
        {
            await ctx.StartDaemonAsync();
        }

        var (code, stdout, stderr) = await ShortcutManager.RunWslWithStdinAsync(
            distro,
            false,
            130,
            actionJson + "\n",
            "focusstage",
            "--json",
            "accounts",
            "apply");

        var msg = ExtractJsonMessage(stdout);
        if (code != 0)
        {
            return new
            {
                ok = false,
                message = !string.IsNullOrWhiteSpace(msg)
                    ? msg
                    : (string.IsNullOrWhiteSpace(stderr) ? $"操作失败 (exit {code})" : stderr),
            };
        }

        string finalMessage = string.IsNullOrWhiteSpace(msg) ? "操作已完成。" : msg;
        if (saveToHello && !string.IsNullOrWhiteSpace(usernameToSync) && !string.IsNullOrEmpty(passwordToSync))
        {
            var existingMeta = CredentialVault.GetMetadata(distro, usernameToSync);
            var saveRes = await CredentialVault.SaveCredentialWithHelloAsync(
                hwnd,
                distro,
                usernameToSync,
                passwordToSync,
                updateLinux: true,
                keyringUsesLinuxPassword: existingMeta.KeyringUsesLinuxPassword,
                keyringPassword: null,
                updateKeyring: false,
                requireHello: true);

            if (saveRes.Status == HelloVerifyStatus.Verified)
            {
                finalMessage += " 已同步保存到 Windows Hello 凭据库。";
            }
            else if (!string.IsNullOrWhiteSpace(saveRes.Message))
            {
                finalMessage += $"（Linux 账号已更新，但未写入 Windows Hello：{saveRes.Message}）";
            }
        }

        _ = isAdminForCreate;
        var view = await GetViewAsync(distro);
        return new { ok = true, message = finalMessage, view };
    }

    private static string ExtractJsonMessage(string json)
    {
        if (string.IsNullOrWhiteSpace(json)) return "";
        try
        {
            using var doc = JsonDocument.Parse(json);
            if (doc.RootElement.TryGetProperty("message", out var el) && el.ValueKind == JsonValueKind.String)
            {
                return el.GetString() ?? "";
            }
        }
        catch
        {
            // Not JSON
        }
        return "";
    }
}
