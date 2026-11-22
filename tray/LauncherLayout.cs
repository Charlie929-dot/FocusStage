using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusStageTray;

internal sealed class LauncherFolder
{
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("app_ids")]
    public List<string> AppIds { get; set; } = new();
}

internal sealed class LauncherLayout
{
    private const string FolderTokenPrefix = "folder:";

    [JsonPropertyName("folders")]
    public List<LauncherFolder> Folders { get; set; } = new();

    [JsonPropertyName("root_order")]
    public List<string> RootOrder { get; set; } = new();

    public static string LayoutFilePath =>
        Path.Combine(ShortcutManager.AppInstallDir, "layout.json");

    public static string FolderToken(string folderId) => FolderTokenPrefix + folderId;

    public static bool IsFolderToken(string token, out string folderId)
    {
        if (token.StartsWith(FolderTokenPrefix, StringComparison.Ordinal))
        {
            folderId = token[FolderTokenPrefix.Length..];
            return true;
        }
        folderId = "";
        return false;
    }

    public static LauncherLayout LoadOrDefault(IReadOnlyList<LinuxAppDto> apps)
    {
        LauncherLayout? loaded = null;
        try
        {
            if (File.Exists(LayoutFilePath))
            {
                var json = File.ReadAllText(LayoutFilePath);
                loaded = JsonSerializer.Deserialize<LauncherLayout>(json);
            }
        }
        catch
        {
            loaded = null;
        }

        if (loaded is null)
        {
            loaded = BuildInitialPhoneLayout(apps);
            loaded.Save();
        }
        else
        {
            if (loaded.ReconcileWithApps(apps))
            {
                loaded.Save();
            }
        }
        return loaded;
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(ShortcutManager.AppInstallDir);
            var json = JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true });
            File.WriteAllText(LayoutFilePath, json);
        }
        catch
        {
            // Ignore persistence errors if disk is read-only.
        }
    }

    private static LauncherLayout BuildInitialPhoneLayout(IReadOnlyList<LinuxAppDto> apps)
    {
        var layout = new LauncherLayout();
        var terminalFolder = new LauncherFolder { Id = "terminals", Name = "终端工具" };
        var systemFolder = new LauncherFolder { Id = "system", Name = "系统与输入法" };
        var downloadsFolder = new LauncherFolder { Id = "downloads", Name = "下载应用" };

        foreach (var app in apps)
        {
            if (string.Equals(app.Source, "deb", StringComparison.OrdinalIgnoreCase)
                || string.Equals(app.Source, "appimage", StringComparison.OrdinalIgnoreCase))
            {
                downloadsFolder.AppIds.Add(app.Id);
                continue;
            }

            var cats = app.Categories ?? "";
            var stem = app.Stem.ToLowerInvariant();
            if (stem.Contains("term") || stem.Contains("foot") || cats.Contains("TerminalEmulator", StringComparison.OrdinalIgnoreCase))
            {
                terminalFolder.AppIds.Add(app.Id);
            }
            else if (stem.Contains("fcitx") || cats.Contains("Settings", StringComparison.OrdinalIgnoreCase) || cats.Contains("System", StringComparison.OrdinalIgnoreCase))
            {
                systemFolder.AppIds.Add(app.Id);
            }
            else
            {
                layout.RootOrder.Add(app.Id);
            }
        }

        if (terminalFolder.AppIds.Count > 0)
        {
            layout.Folders.Add(terminalFolder);
            layout.RootOrder.Add(FolderToken(terminalFolder.Id));
        }
        if (systemFolder.AppIds.Count > 0)
        {
            layout.Folders.Add(systemFolder);
            layout.RootOrder.Add(FolderToken(systemFolder.Id));
        }
        if (downloadsFolder.AppIds.Count > 0)
        {
            layout.Folders.Add(downloadsFolder);
            layout.RootOrder.Add(FolderToken(downloadsFolder.Id));
        }

        return layout;
    }

    /// <summary>
    /// Ensures newly installed/downloaded apps appear automatically and uninstalled apps are cleaned up.
    /// Returns true if the layout changed.
    /// </summary>
    public bool ReconcileWithApps(IReadOnlyList<LinuxAppDto> apps)
    {
        bool changed = false;
        var validIds = new HashSet<string>(apps.Select(a => a.Id), StringComparer.Ordinal);
        var folderIds = new HashSet<string>(Folders.Select(f => f.Id), StringComparer.Ordinal);

        // Remove apps that no longer exist from folders and root
        foreach (var folder in Folders)
        {
            int before = folder.AppIds.Count;
            folder.AppIds = folder.AppIds.Where(validIds.Contains).Distinct(StringComparer.Ordinal).ToList();
            if (folder.AppIds.Count != before) changed = true;
        }

        var newRoot = new List<string>();
        var seenRoot = new HashSet<string>(StringComparer.Ordinal);
        foreach (var tok in RootOrder)
        {
            if (IsFolderToken(tok, out var fid))
            {
                if (folderIds.Contains(fid) && seenRoot.Add(tok))
                {
                    newRoot.Add(tok);
                }
                else
                {
                    changed = true;
                }
            }
            else if (validIds.Contains(tok) && seenRoot.Add(tok))
            {
                newRoot.Add(tok);
            }
            else
            {
                changed = true;
            }
        }
        RootOrder = newRoot;

        // Ensure every folder has a token in RootOrder
        foreach (var folder in Folders)
        {
            var tok = FolderToken(folder.Id);
            if (!seenRoot.Contains(tok))
            {
                RootOrder.Add(tok);
                seenRoot.Add(tok);
                changed = true;
            }
        }

        // Collect all already-placed app IDs
        var placed = new HashSet<string>(StringComparer.Ordinal);
        foreach (var folder in Folders)
        {
            foreach (var id in folder.AppIds) placed.Add(id);
        }
        foreach (var tok in RootOrder)
        {
            if (!IsFolderToken(tok, out _)) placed.Add(tok);
        }

        // Add newly installed or browser-downloaded apps right to the home grid so user sees them immediately
        foreach (var app in apps)
        {
            if (placed.Add(app.Id))
            {
                RootOrder.Add(app.Id);
                changed = true;
            }
        }

        return changed;
    }

    public LauncherFolder? FindFolder(string folderId) =>
        Folders.FirstOrDefault(f => f.Id == folderId);

    public LauncherFolder CreateFolder(string name, string? initialAppId = null)
    {
        var id = "f_" + Guid.NewGuid().ToString("N")[..8];
        var folder = new LauncherFolder
        {
            Id = id,
            Name = string.IsNullOrWhiteSpace(name) ? "新建文件夹" : name.Trim()
        };
        Folders.Add(folder);
        RootOrder.Add(FolderToken(id));

        if (!string.IsNullOrWhiteSpace(initialAppId))
        {
            MoveAppToFolder(initialAppId, id);
        }
        Save();
        return folder;
    }

    public void RenameFolder(string folderId, string newName)
    {
        var folder = FindFolder(folderId);
        if (folder is null || string.IsNullOrWhiteSpace(newName)) return;
        folder.Name = newName.Trim();
        Save();
    }

    public void DissolveFolder(string folderId)
    {
        var folder = FindFolder(folderId);
        if (folder is null) return;
        var tok = FolderToken(folderId);
        int idx = RootOrder.IndexOf(tok);
        if (idx >= 0)
        {
            RootOrder.RemoveAt(idx);
            RootOrder.InsertRange(idx, folder.AppIds);
        }
        else
        {
            RootOrder.AddRange(folder.AppIds);
        }
        Folders.Remove(folder);
        Save();
    }

    public void MoveAppToFolder(string appId, string targetFolderId)
    {
        var target = FindFolder(targetFolderId);
        if (target is null) return;

        RemoveAppFromAllLocations(appId);
        if (!target.AppIds.Contains(appId))
        {
            target.AppIds.Add(appId);
        }
        Save();
    }

    public void MoveAppToRoot(string appId)
    {
        RemoveAppFromAllLocations(appId);
        RootOrder.Add(appId);
        Save();
    }

    public LauncherFolder MergeAppsIntoNewFolder(string sourceAppId, string targetAppId, string folderName = "新建文件夹")
    {
        var targetFolder = CreateFolder(folderName);
        int targetIdx = RootOrder.IndexOf(targetAppId);
        if (targetIdx >= 0)
        {
            RootOrder.Remove(FolderToken(targetFolder.Id));
            RootOrder.Insert(targetIdx, FolderToken(targetFolder.Id));
        }

        RemoveAppFromAllLocations(targetAppId);
        RemoveAppFromAllLocations(sourceAppId);
        targetFolder.AppIds.Add(targetAppId);
        if (sourceAppId != targetAppId)
        {
            targetFolder.AppIds.Add(sourceAppId);
        }
        Save();
        return targetFolder;
    }

    public void AutoOrganize(IReadOnlyList<LinuxAppDto> apps)
    {
        var fresh = BuildInitialPhoneLayout(apps);
        Folders = fresh.Folders;
        RootOrder = fresh.RootOrder;
        Save();
    }

    private void RemoveAppFromAllLocations(string appId)
    {
        RootOrder.RemoveAll(t => t == appId);
        foreach (var f in Folders)
        {
            f.AppIds.RemoveAll(id => id == appId);
        }
    }
}
