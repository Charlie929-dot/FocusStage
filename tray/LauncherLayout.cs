using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusStageTray;

internal sealed class LauncherGridPosition
{
    [JsonPropertyName("row")]
    public int Row { get; set; }

    [JsonPropertyName("col")]
    public int Col { get; set; }
}

internal sealed class LauncherDockItem
{
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("kind")]
    public string Kind { get; set; } = "app";

    [JsonPropertyName("target")]
    public string Target { get; set; } = "";

    [JsonPropertyName("label")]
    public string Label { get; set; } = "";
}

internal sealed class LauncherAppearance
{
    private static readonly HashSet<string> Themes = new(new[]
    {
        "mist", "ink", "paper", "forest", "violet"
    }, StringComparer.OrdinalIgnoreCase);

    private static readonly HashSet<string> FontPresets = new(new[]
    {
        "system", "rounded", "serif", "mono", "custom"
    }, StringComparer.OrdinalIgnoreCase);

    private static readonly Dictionary<string, (string Bg, string Surface, string Ink, string Accent)> ThemeColors = new(StringComparer.OrdinalIgnoreCase)
    {
        ["mist"] = ("#e9edf3", "#f9fbfd", "#1b263b", "#3d66d5"),
        ["ink"] = ("#101725", "#182133", "#e8eef8", "#8ba8ff"),
        ["paper"] = ("#efe9dd", "#faf7ef", "#302b24", "#a15d3b"),
        ["forest"] = ("#0e1d1a", "#162925", "#e4f2ea", "#5fc2a2"),
        ["violet"] = ("#19162b", "#241f3c", "#eeeaff", "#b79aff")
    };

    [JsonPropertyName("theme")]
    public string Theme { get; set; } = "mist";

    [JsonPropertyName("customColors")]
    public bool CustomColors { get; set; }

    [JsonPropertyName("background")]
    public string Background { get; set; } = "#e9edf3";

    [JsonPropertyName("surface")]
    public string Surface { get; set; } = "#f9fbfd";

    [JsonPropertyName("ink")]
    public string Ink { get; set; } = "#1b263b";

    [JsonPropertyName("accent")]
    public string Accent { get; set; } = "#3d66d5";

    [JsonPropertyName("fontPreset")]
    public string FontPreset { get; set; } = "system";

    [JsonPropertyName("customFont")]
    public string CustomFont { get; set; } = "";

    [JsonPropertyName("fontSize")]
    public int FontSize { get; set; } = 13;

    [JsonPropertyName("columns")]
    public int Columns { get; set; } = 8;

    [JsonPropertyName("tileScale")]
    public int TileScale { get; set; } = 100;

    [JsonPropertyName("showGrid")]
    public bool ShowGrid { get; set; } = false;

    [JsonPropertyName("showDock")]
    public bool ShowDock { get; set; } = true;

    [JsonPropertyName("dockLabels")]
    public bool DockLabels { get; set; } = true;

    public void Normalize()
    {
        var theme = (Theme ?? "").ToLowerInvariant();
        Theme = Themes.Contains(theme) ? theme : "mist";
        var defaults = ThemeColors[Theme];

        Background = NormalizeHex(Background, defaults.Bg);
        Surface = NormalizeHex(Surface, defaults.Surface);
        Ink = NormalizeHex(Ink, defaults.Ink);
        Accent = NormalizeHex(Accent, defaults.Accent);

        var fontPreset = (FontPreset ?? "").ToLowerInvariant();
        FontPreset = FontPresets.Contains(fontPreset) ? fontPreset : "system";
        CustomFont = (CustomFont ?? "").Trim();
        if (CustomFont.Length > 180) CustomFont = CustomFont[..180];
        FontSize = FontSize == 0 ? 13 : Math.Clamp(FontSize, 11, 18);
        Columns = Columns == 0 ? 8 : Math.Clamp(Columns, 6, 12);
        TileScale = TileScale == 0 ? 100 : Math.Clamp(TileScale, 80, 130);
    }

    private static string NormalizeHex(string? value, string fallback)
    {
        var text = (value ?? "").Trim();
        if (!text.StartsWith('#')) text = "#" + text;
        if ((text.Length == 7 || text.Length == 4) && text[1..].All(Uri.IsHexDigit))
        {
            return text.ToLowerInvariant();
        }
        return fallback;
    }
}

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

    private static readonly HashSet<string> AllowedDockTargets = new(new[]
    {
        "view:stages", "view:daemon", "view:linux", "view:appearance",
        "action:appCenter", "action:newFolder", "action:runCommand",
        "action:refreshApps", "action:autoOrganize", "action:syncStartMenu"
    }, StringComparer.Ordinal);

    private static readonly Dictionary<string, string> DockTargetLabels = new(StringComparer.Ordinal)
    {
        ["view:stages"] = "舞台中心",
        ["view:daemon"] = "守护设置",
        ["view:linux"] = "Linux 设置",
        ["view:appearance"] = "外观设置",
        ["action:appCenter"] = "应用中心",
        ["action:newFolder"] = "新建文件夹",
        ["action:runCommand"] = "运行命令",
        ["action:refreshApps"] = "刷新应用",
        ["action:autoOrganize"] = "整理桌面",
        ["action:syncStartMenu"] = "同步快捷方式"
    };

    [JsonPropertyName("folders")]
    public List<LauncherFolder> Folders { get; set; } = new();

    [JsonPropertyName("root_order")]
    public List<string> RootOrder { get; set; } = new();

    [JsonPropertyName("positions")]
    public Dictionary<string, LauncherGridPosition> Positions { get; set; } = new(StringComparer.Ordinal);

    [JsonPropertyName("dock_items")]
    public List<LauncherDockItem> DockItems { get; set; } = new();

    [JsonPropertyName("appearance")]
    public LauncherAppearance Appearance { get; set; } = new();

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
            return loaded;
        }

        loaded.Folders ??= new();
        loaded.RootOrder ??= new();
        loaded.Positions = loaded.Positions is null
            ? new Dictionary<string, LauncherGridPosition>(StringComparer.Ordinal)
            : new Dictionary<string, LauncherGridPosition>(loaded.Positions, StringComparer.Ordinal);
        loaded.DockItems ??= new();
        loaded.Appearance ??= new();
        loaded.Appearance.Normalize();

        if (loaded.ReconcileWithApps(apps))
        {
            loaded.Save();
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

        layout.EnsurePositions(layout.Appearance.Columns);
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

        var placed = new HashSet<string>(StringComparer.Ordinal);
        foreach (var folder in Folders)
        {
            foreach (var id in folder.AppIds) placed.Add(id);
        }
        foreach (var tok in RootOrder)
        {
            if (!IsFolderToken(tok, out _)) placed.Add(tok);
        }

        foreach (var app in apps)
        {
            if (placed.Add(app.Id))
            {
                RootOrder.Add(app.Id);
                changed = true;
            }
        }

        changed |= PruneDockItems(validIds, folderIds);
        changed |= EnsurePositions(Appearance.Columns);
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
            EnsurePositions(Appearance.Columns);
        }
        else
        {
            EnsurePositions(Appearance.Columns);
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
        EnsurePositions(Appearance.Columns);
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
        EnsurePositions(Appearance.Columns);
        Save();
    }

    public void MoveAppToRoot(string appId)
    {
        RemoveAppFromAllLocations(appId);
        RootOrder.Add(appId);
        EnsurePositions(Appearance.Columns);
        Save();
    }

    public void MoveAppToRootAt(string appId, int row, int col)
    {
        MoveAppToRoot(appId);
        SetRootPosition(appId, row, col);
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
        EnsurePositions(Appearance.Columns);
        Save();
        return targetFolder;
    }

    public void AutoOrganize(IReadOnlyList<LinuxAppDto> apps)
    {
        var fresh = BuildInitialPhoneLayout(apps);
        Folders = fresh.Folders;
        RootOrder = fresh.RootOrder;
        Positions.Clear();
        EnsurePositions(Appearance.Columns);
        Save();
    }

    public void ReorderRoot(IReadOnlyList<string> newOrder)
    {
        var existing = new HashSet<string>(RootOrder, StringComparer.Ordinal);
        var updated = new List<string>(RootOrder.Count);
        var seen = new HashSet<string>(StringComparer.Ordinal);

        foreach (var token in newOrder)
        {
            if (existing.Contains(token) && seen.Add(token))
            {
                updated.Add(token);
            }
        }
        foreach (var token in RootOrder)
        {
            if (seen.Add(token))
            {
                updated.Add(token);
            }
        }
        RootOrder = updated;
        Positions.Clear();
        EnsurePositions(Appearance.Columns);
        Save();
    }

    public void ReorderFolder(string folderId, IReadOnlyList<string> newAppIds)
    {
        var folder = FindFolder(folderId);
        if (folder is null) return;

        var existing = new HashSet<string>(folder.AppIds, StringComparer.Ordinal);
        var updated = new List<string>(folder.AppIds.Count);
        var seen = new HashSet<string>(StringComparer.Ordinal);

        foreach (var id in newAppIds)
        {
            if (existing.Contains(id) && seen.Add(id))
            {
                updated.Add(id);
            }
        }
        foreach (var id in folder.AppIds)
        {
            if (seen.Add(id))
            {
                updated.Add(id);
            }
        }
        folder.AppIds = updated;
        Save();
    }

    public void SetRootPosition(string token, int row, int col)
    {
        if (!RootOrder.Contains(token, StringComparer.Ordinal)) return;
        EnsurePositions(Appearance.Columns);

        row = Math.Max(0, row);
        col = Math.Clamp(col, 0, Appearance.Columns - 1);
        var old = Positions[token];
        if (old.Row == row && old.Col == col) return;

        string? occupant = null;
        foreach (var pair in Positions)
        {
            if (!string.Equals(pair.Key, token, StringComparison.Ordinal)
                && pair.Value.Row == row && pair.Value.Col == col)
            {
                occupant = pair.Key;
                break;
            }
        }

        Positions[token] = new LauncherGridPosition { Row = row, Col = col };
        if (occupant is not null)
        {
            Positions[occupant] = new LauncherGridPosition { Row = old.Row, Col = old.Col };
        }
        Save();
    }

    public void SetDockItems(
        IReadOnlyList<LauncherDockItem> items,
        IReadOnlyDictionary<string, LinuxAppDto> appLookup)
    {
        var folderIds = new HashSet<string>(Folders.Select(f => f.Id), StringComparer.Ordinal);
        var seenTargets = new HashSet<string>(StringComparer.Ordinal);
        var seenIds = new HashSet<string>(StringComparer.Ordinal);
        var cleaned = new List<LauncherDockItem>();

        foreach (var source in items.Take(20))
        {
            var kind = (source.Kind ?? "").Trim().ToLowerInvariant();
            var target = (source.Target ?? "").Trim();
            if (!IsValidDockTarget(kind, target, appLookup, folderIds)) continue;
            if (!seenTargets.Add($"{kind}:{target}")) continue;

            var id = string.IsNullOrWhiteSpace(source.Id)
                ? "d_" + Guid.NewGuid().ToString("N")[..8]
                : source.Id.Trim();
            if (!seenIds.Add(id)) id = "d_" + Guid.NewGuid().ToString("N")[..8];

            var inferred = InferDockLabel(kind, target, appLookup);
            var label = string.IsNullOrWhiteSpace(source.Label) ? inferred : source.Label.Trim();
            if (label.Length > 28) label = label[..28];

            cleaned.Add(new LauncherDockItem
            {
                Id = id,
                Kind = kind,
                Target = target,
                Label = label
            });
        }

        DockItems = cleaned;
        Save();
    }

    public void SetAppearance(LauncherAppearance? appearance)
    {
        if (appearance is not null)
        {
            Appearance = appearance;
        }
        Appearance.Normalize();
        EnsurePositions(Appearance.Columns);
        Save();
    }

    private bool PruneDockItems(HashSet<string> validAppIds, HashSet<string> folderIds)
    {
        var before = DockItems.Count;
        DockItems = DockItems
            .Where(item => IsValidDockTarget(
                (item.Kind ?? "").Trim().ToLowerInvariant(),
                (item.Target ?? "").Trim(),
                validAppIds,
                folderIds))
            .ToList();
        return DockItems.Count != before;
    }

    private static bool IsValidDockTarget(
        string kind,
        string target,
        IReadOnlyCollection<string> validAppIds)
    {
        if (kind == "app") return validAppIds.Contains(target);
        if (kind == "view" || kind == "action")
        {
            return AllowedDockTargets.Contains(target)
                   && target.StartsWith(kind + ":", StringComparison.Ordinal);
        }
        return false;
    }

    private bool IsValidDockTarget(
        string kind,
        string target,
        IReadOnlyDictionary<string, LinuxAppDto> appLookup,
        HashSet<string> folderIds)
    {
        if (kind == "app") return appLookup.ContainsKey(target);
        if (kind == "folder") return folderIds.Contains(target);
        return IsValidDockTarget(kind, target, Array.Empty<string>()) || AllowedDockTargets.Contains(target);
    }

    private static bool IsValidDockTarget(
        string kind,
        string target,
        IReadOnlyCollection<string> validAppIds,
        HashSet<string> folderIds)
    {
        if (kind == "app") return validAppIds.Contains(target);
        if (kind == "folder") return folderIds.Contains(target);
        return IsValidDockTarget(kind, target, Array.Empty<string>());
    }

    private string InferDockLabel(
        string kind,
        string target,
        IReadOnlyDictionary<string, LinuxAppDto> appLookup)
    {
        if (kind == "app" && appLookup.TryGetValue(target, out var app)) return app.DisplayName;
        if (kind == "folder") return FindFolder(target)?.Name ?? "文件夹";
        return DockTargetLabels.TryGetValue(target, out var label) ? label : target;
    }

    private bool EnsurePositions(int requestedColumns)
    {
        int columns = Math.Clamp(requestedColumns, 6, 12);
        var occupied = new HashSet<(int Row, int Col)>();
        var rebuilt = new Dictionary<string, LauncherGridPosition>(StringComparer.Ordinal);
        bool changed = Positions.Count != RootOrder.Count;

        foreach (var token in RootOrder)
        {
            if (Positions.TryGetValue(token, out var pos)
                && pos.Row >= 0
                && pos.Col >= 0
                && pos.Col < columns
                && occupied.Add((pos.Row, pos.Col)))
            {
                rebuilt[token] = new LauncherGridPosition { Row = pos.Row, Col = pos.Col };
            }
            else
            {
                var free = FindFirstFreeCell(occupied, columns);
                rebuilt[token] = new LauncherGridPosition { Row = free.Row, Col = free.Col };
                occupied.Add(free);
                changed = true;
            }
        }

        foreach (var token in Positions.Keys)
        {
            if (!RootOrder.Contains(token, StringComparer.Ordinal))
            {
                changed = true;
                break;
            }
        }

        Positions = rebuilt;
        return changed;
    }

    private static (int Row, int Col) FindFirstFreeCell(HashSet<(int Row, int Col)> occupied, int columns)
    {
        int row = 0;
        int col = 0;
        while (occupied.Contains((row, col)))
        {
            col++;
            if (col >= columns)
            {
                col = 0;
                row++;
            }
        }
        return (row, col);
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
