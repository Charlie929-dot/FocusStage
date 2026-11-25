using Windows.Security.Credentials.UI;
using WinRT.Interop;

namespace FocusStageTray;

internal enum LauncherAuthResult
{
    Verified,
    Canceled,
    HelloUnavailable,
    LinuxUnavailable,
    LinuxAccountUnavailable,
}

internal static class LauncherAuthentication
{
    public static async Task<LauncherAuthResult> VerifyHelloAsync(nint window, CancellationToken cancellation)
    {
        try
        {
            var availability = await UserConsentVerifier.CheckAvailabilityAsync().AsTask(cancellation);
            if (availability != UserConsentVerifierAvailability.Available)
            {
                return LauncherAuthResult.HelloUnavailable;
            }

            var result = await UserConsentVerifierInterop.RequestVerificationForWindowAsync(
                window, "验证身份以解锁 FocusStage 启动器").AsTask(cancellation);
            return result switch
            {
                UserConsentVerificationResult.Verified => LauncherAuthResult.Verified,
                UserConsentVerificationResult.Canceled or UserConsentVerificationResult.RetriesExhausted
                    => LauncherAuthResult.Canceled,
                _ => LauncherAuthResult.HelloUnavailable,
            };
        }
        catch (Exception ex) when (ex is System.Runtime.InteropServices.COMException or NotSupportedException)
        {
            return LauncherAuthResult.HelloUnavailable;
        }
    }

    public static async Task<LauncherAuthResult> VerifyLinuxAsync(string distro, uint uid, CancellationToken cancellation)
    {
        var (code, account, _) = uid == 0
            ? await ShortcutManager.RunWslAsync(distro, true, "/usr/bin/python3", "-I", UbuntuSessionAuthorization.HelperPath, "verification-user").WaitAsync(cancellation)
            : await ShortcutManager.RunWslAsync(distro, false, "/usr/bin/getent", "passwd",
                uid.ToString(System.Globalization.CultureInfo.InvariantCulture)).WaitAsync(cancellation);
        var user = uid == 0 ? account : account.Split(':', 2)[0];
        if (code != 0 || string.IsNullOrWhiteSpace(user)) return LauncherAuthResult.LinuxAccountUnavailable;

        // pkexec skips authentication for UID 0. Resolve and verify the actual prompt account first.
        var (idCode, id, _) = await ShortcutManager.RunWslAsync(distro, user, 15, "/usr/bin/id", "-u").WaitAsync(cancellation);
        if (idCode != 0 || !uint.TryParse(id, out var promptUid) || promptUid == 0 || (uid != 0 && uid != promptUid))
            return LauncherAuthResult.LinuxAccountUnavailable;

        cancellation.ThrowIfCancellationRequested();
        // Ensure the FocusStage native Polkit agent is registered before pkexec requests auth.
        var (agentCode, agentOutput, _) = await ShortcutManager.RunWslAsync(
            distro, user, 10, "/usr/local/bin/focusstage", "--json", "ensure-polkit-agent").WaitAsync(cancellation);
        if (agentCode == 0 && agentOutput.Contains("\"ready\":true", StringComparison.Ordinal))
        {
            await Task.Delay(50, cancellation);
        }

        const string confirmation = "FocusStageVerified";
        for (var attempt = 0; attempt < 2; attempt++)
        {
            cancellation.ThrowIfCancellationRequested();
            var unit = $"focusstage-launcher-auth-{Guid.NewGuid():N}.service";
            var startedAt = Environment.TickCount64;
            var verification = ShortcutManager.RunWslAsync(distro, user, 100,
                "/usr/bin/systemd-run", "--user", "--wait", "--pipe", "--quiet", "--collect",
                $"--unit={unit}", "--property=RuntimeMaxSec=90s",
                "/usr/bin/pkexec", "--disable-internal-agent", UbuntuSessionAuthorization.HelperPath, "verify");
            try
            {
                var (resultCode, output, _) = await verification.WaitAsync(cancellation);
                // If the Polkit agent just started and hadn't registered on D-Bus yet, pkexec fails
                // immediately with 127; give it a brief moment and retry once.
                if (resultCode == 127 && attempt == 0 && Environment.TickCount64 - startedAt < 2500)
                {
                    await Task.Delay(350, cancellation);
                    continue;
                }
                // Stopping a systemd service can report success without running the authorized command.
                return resultCode switch
                {
                    0 when output == confirmation => LauncherAuthResult.Verified,
                    0 or 126 => LauncherAuthResult.Canceled,
                    _ => LauncherAuthResult.LinuxUnavailable,
                };
            }
            finally
            {
                if (cancellation.IsCancellationRequested)
                {
                    _ = ShortcutManager.RunWslAsync(distro, user, 15, "/usr/bin/systemctl", "--user", "stop", unit);
                }
            }
        }
        return LauncherAuthResult.LinuxUnavailable;
    }
}
