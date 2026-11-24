using System.Diagnostics;
using System.Globalization;

namespace FocusStageTray;

internal sealed class UbuntuSessionAuthorization(Process holder) : IDisposable
{
    public const string HelperPath = "/usr/local/libexec/focusstage-auth";

    public static async Task<uint> GetDefaultUserIdAsync(string distro, CancellationToken cancellation)
    {
        var (code, account, _) = await ShortcutManager.RunWslAsync(distro, false, "/usr/bin/id", "-u")
            .WaitAsync(cancellation);
        if (code != 0 || !uint.TryParse(account, NumberStyles.None, CultureInfo.InvariantCulture, out var uid))
        {
            throw new InvalidOperationException("无法确认 Ubuntu 用户。");
        }
        return uid;
    }

    public static async Task<UbuntuSessionAuthorization?> OpenAsync(string distro, uint uid, CancellationToken cancellation)
    {
        if (uid == 0) return null;

        var start = new ProcessStartInfo(Path.Combine(Environment.SystemDirectory, "wsl.exe"))
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardInput = true,
            StandardInputEncoding = new System.Text.UTF8Encoding(encoderShouldEmitUTF8Identifier: false),
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        foreach (var arg in new[] { "-d", distro, "-u", "root", "-e", "/usr/bin/python3", "-I", HelperPath, "hold", uid.ToString(CultureInfo.InvariantCulture) })
        {
            start.ArgumentList.Add(arg);
        }

        cancellation.ThrowIfCancellationRequested();
        var process = Process.Start(start) ?? throw new InvalidOperationException("无法启动 Ubuntu 会话授权。");
        var authorization = new UbuntuSessionAuthorization(process);
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            timeout.CancelAfter(TimeSpan.FromSeconds(12));
            var ready = await process.StandardOutput.ReadLineAsync(timeout.Token);
            if (ready != "FocusStageAuthorizationReady")
            {
                throw new InvalidOperationException("无法启用 Ubuntu 会话授权。请重新运行 FocusStage 安装器。");
            }
            return authorization;
        }
        catch
        {
            authorization.Dispose();
            throw;
        }
    }

    public Task WaitForEndAsync(CancellationToken cancellation) => holder.WaitForExitAsync(cancellation);

    public void Dispose()
    {
        try
        {
            holder.StandardInput.Close();
        }
        catch (IOException)
        {
            // WSL may already have closed its end of the pipe.
        }
        finally
        {
            holder.Dispose();
        }
    }
}
