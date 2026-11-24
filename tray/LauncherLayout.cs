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
    private static readonly object FileLock = new();
    private static readonly JsonSerializerOptions WriteJsonOptions = new() { WriteIndented = true };

    private enum ReadFileStatus
    {
        Missing,
        Success,
        Corrupt,
        IoError
    }

    private bool _saveBlockedByIoError;
    private bool _loadedFromCorruptPrimary;

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

    public static string BackupFilePath =>
        Path.Combine(ShortcutManager.AppInstallDir, "layout.json.bak");

    private static string TempFilePath =>
        Path.Combine(ShortcutManager.AppInstallDir, "layout.json.tmp");

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

    /// <summary>
    /// Pre-loads existing layout.json (or layout.json.bak) without pruning or writing to disk.
    /// Safe to call at startup before Linux apps have been scanned.
    /// </summary>
    public static LauncherLayout LoadExistingReadOnly()
    {
        lock (FileLock)
        {
            var primaryStatus = TryReadLayoutFile(LayoutFilePath, out var primary);
            if (primaryStatus == ReadFileStatus.Success && primary is not null)
            {
                return primary;
            }

            var backupStatus = TryReadLayoutFile(BackupFilePath, out var backup);
            if (backupStatus == ReadFileStatus.Success && backup is not null)
            {
                backup._saveBlockedByIoError = primaryStatus == ReadFileStatus.IoError;
                backup._loadedFromCorruptPrimary = primaryStatus == ReadFileStatus.Corrupt;
                return backup;
            }

            var fallback = new LauncherLayout
            {
                _saveBlockedByIoError = primaryStatus == ReadFileStatus.IoError,
                _loadedFromCorruptPrimary = primaryStatus == ReadFileStatus.Corrupt
            };
            fallback.NormalizeLoaded();
            return fallback;
        }
    }

    public static LauncherLayout LoadOrDefault(IReadOnlyList<LinuxAppDto> apps)
    {
        if (apps.Count == 0)
        {
            return LoadExistingReadOnly();
        }

        lock (FileLock)
        {
            var primaryStatus = TryReadLayoutFile(LayoutFilePath, out var primary);
            if (primaryStatus == ReadFileStatus.Success && primary is not null)
            {
                if (primary.ReconcileWithApps(apps))
                {
                    primary.Save();
                }
                return primary;
            }

            if (primaryStatus == ReadFileStatus.IoError)
            {
                // Transient lock/access error on an existing layout.json: never overwrite it.
                var backupStatus = TryReadLayoutFile(BackupFilePath, out var backup);
                var safeLayout = (backupStatus == ReadFileStatus.Success && backup is not null)
                    ? backup
                    : BuildInitialPhoneLayout(apps);
                safeLayout._saveBlockedByIoError = true;
                safeLayout.ReconcileWithApps(apps);
                return safeLayout;
            }

            if (primaryStatus == ReadFileStatus.Corrupt)
            {
                var backupStatus = TryReadLayoutFile(BackupFilePath, out var backup);
                PreserveCorruptPrimaryLocked();

                if (backupStatus == ReadFileStatus.Success && backup is not null)
                {
                    backup.ReconcileWithApps(apps);
                    backup.Save();
                    return backup;
                }

                var freshAfterCorrupt = BuildInitialPhoneLayout(apps);
                freshAfterCorrupt.Save();
                return freshAfterCorrupt;
            }

            // Primary missing: try recovering from layout.json.bak before creating defaults.
            var missingFallbackStatus = TryReadLayoutFile(BackupFilePath, out var recoveredBackup);
            if (missingFallbackStatus == ReadFileStatus.Success && recoveredBackup is not null)
            {
                recoveredBackup.ReconcileWithApps(apps);
                recoveredBackup.Save();
                return recoveredBackup;
            }

            var fresh = BuildInitialPhoneLayout(apps);
            fresh.Save();
            return fresh;
        }
    }

    private static ReadFileStatus TryReadLayoutFile(string path, out LauncherLayout? layout)
    {
        layout = null;
        if (!File.Exists(path))
        {
            return ReadFileStatus.Missing;
        }

        string? json = null;
        for (int attempt = 0; attempt < 3; attempt++)
        {
            try
            {
                using var fs = new FileStream(
                    path,
                    FileMode.Open,
                    FileAccess.Read,
                    FileShare.ReadWrite | FileShare.Delete);
                using var reader = new StreamReader(fs, System.Text.Encoding.UTF8);
                json = reader.ReadToEnd();
                break;
            }
            catch (FileNotFoundException)
            {
                return ReadFileStatus.Missing;
            }
            catch (DirectoryNotFoundException)
            {
                return ReadFileStatus.Missing;
            }
            catch (IOException) when (attempt < 2)
            {
                Thread.Sleep(40 * (attempt + 1));
            }
            catch (UnauthorizedAccessException) when (attempt < 2)
            {
                Thread.Sleep(40 * (attempt + 1));
            }
            catch (IOException)
            {
                return ReadFileStatus.IoError;
            }
            catch (UnauthorizedAccessException)
            {
                return ReadFileStatus.IoError;
            }
        }

        if (string.IsNullOrWhiteSpace(json))
        {
            return ReadFileStatus.Corrupt;
        }

        try
        {
            var parsed = JsonSerializer.Deserialize<LauncherLayout>(json);
            if (parsed is null)
            {
                return ReadFileStatus.Corrupt;
            }
            parsed.NormalizeLoaded();
            layout = parsed;
            return ReadFileStatus.Success;
        }
        catch (JsonException)
        {
            return ReadFileStatus.Corrupt;
        }
    }

    private static void PreserveCorruptPrimaryLocked()
    {
        try
        {
            if (!File.Exists(LayoutFilePath)) return;
            var stamp = DateTime.Now.ToString("yyyyMMdd-HHmmss");
            var corruptPath = Path.Combine(ShortcutManager.AppInstallDir, $"layout.json.corrupt.{stamp}");
            if (File.Exists(corruptPath))
            {
                corruptPath = Path.Combine(
                    ShortcutManager.AppInstallDir,
                    $"layout.json.corrupt.{stamp}-{Guid.NewGuid().ToString("N")[..4]}");
            }
            File.Move(LayoutFilePath, corruptPath);
        }
        catch
        {
            try
            {
                var fallbackCorrupt = Path.Combine(ShortcutManager.AppInstallDir, "layout.json.corrupt");
                File.Copy(LayoutFilePath, fallbackCorrupt, overwrite: true);
            }
            catch
            {
                // Ignore if disk is read-only.
            }
        }
    }

    private void NormalizeLoaded()
    {
        Folders = (Folders ?? new())
            .Where(f => f is not null)
            .Select(f =>
            {
                f.Id = (f.Id ?? "").Trim();
                f.Name = string.IsNullOrWhiteSpace(f.Name) ? "新建文件夹" : f.Name.Trim();
                f.AppIds = (f.AppIds ?? new())
                    .Where(id => !string.IsNullOrWhiteSpace(id))
                    .Distinct(StringComparer.Ordinal)
                    .ToList();
                return f;
            })
            .Where(f => !string.IsNullOrWhiteSpace(f.Id))
            .ToList();

        RootOrder = (RootOrder ?? new())
            .Where(t => !string.IsNullOrWhiteSpace(t))
            .ToList();

        Positions = Positions is null
            ? new Dictionary<string, LauncherGridPosition>(StringComparer.Ordinal)
            : new Dictionary<string, LauncherGridPosition>(Positions, StringComparer.Ordinal);

        DockItems = (DockItems ?? new())
            .Where(d => d is not null)
            .ToList();

        Appearance ??= new();
        Appearance.Normalize();
    }

    public void Save()
    {
        if (_saveBlockedByIoError)
        {
            return;
        }

        lock (FileLock)
        {
            try
            {
                Directory.CreateDirectory(ShortcutManager.AppInstallDir);
                var json = JsonSerializer.Serialize(this, WriteJsonOptions);
                var bytes = System.Text.Encoding.UTF8.GetBytes(json);

                using (var fs = new FileStream(
                    TempFilePath,
                    FileMode.Create,
                    FileAccess.Write,
                    FileShare.None))
                {
                    fs.Write(bytes, 0, bytes.Length);
                    fs.Flush(flushToDisk: true);
                }

                if (_loadedFromCorruptPrimary && File.Exists(LayoutFilePath))
                {
                    PreserveCorruptPrimaryLocked();
                    _loadedFromCorruptPrimary = false;
                }

                if (File.Exists(LayoutFilePath))
                {
                    try
                    {
                        File.Replace(TempFilePath, LayoutFilePath, BackupFilePath, ignoreMetadataErrors: true);
                    }
                    catch (FileNotFoundException)
                    {
                        File.Move(TempFilePath, LayoutFilePath, overwrite: true);
                        TryCopyBackupLocked();
                    }
                    catch (IOException)
                    {
                        File.Move(TempFilePath, LayoutFilePath, overwrite: true);
                        TryCopyBackupLocked();
                    }
                }
                else
                {
                    File.Move(TempFilePath, LayoutFilePath, overwrite: true);
                    TryCopyBackupLocked();
                }
            }
            catch
            {
                // Ignore persistence errors if disk is read-only.
            }
            finally
            {
                try
                {
                    if (File.Exists(TempFilePath))
                    {
                        File.Delete(TempFilePath);
                    }
                }
                catch
                {
                    // Ignore temp cleanup errors.
                }
            }
        }
    }

    private static void TryCopyBackupLocked()
    {
        try
        {
            if (File.Exists(LayoutFilePath))
            {
                File.Copy(LayoutFilePath, BackupFilePath, overwrite: true);
            }
        }
        catch
        {
            // Best-effort backup copy.
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
    /// Non-destructively reconciles newly discovered apps into free grid cells without
    /// purging existing custom folders, cell placements, or pinned dock items.
    /// Returns true if the layout changed.
    /// </summary>
    public bool ReconcileWithApps(IReadOnlyList<LinuxAppDto> apps)
    {
        if (apps.Count == 0)
        {
            return false;
        }

        bool changed = false;
        var folderIds = new HashSet<string>(Folders.Select(f => f.Id), StringComparer.Ordinal);

        foreach (var folder in Folders)
        {
            var cleaned = folder.AppIds
                .Where(id => !string.IsNullOrWhiteSpace(id))
                .Distinct(StringComparer.Ordinal)
                .ToList();
            if (cleaned.Count != folder.AppIds.Count || !cleaned.SequenceEqual(folder.AppIds, StringComparer.Ordinal))
            {
                folder.AppIds = cleaned;
                changed = true;
            }
        }

        var newRoot = new List<string>(RootOrder.Count);
        var seenRoot = new HashSet<string>(StringComparer.Ordinal);
        foreach (var tok in RootOrder)
        {
            if (string.IsNullOrWhiteSpace(tok))
            {
                changed = true;
                continue;
            }

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
            else if (seenRoot.Add(tok))
            {
                // Preserve saved app tokens even if temporarily absent from the latest scan.
                newRoot.Add(tok);
            }
            else
            {
                changed = true;
            }
        }
        if (newRoot.Count != RootOrder.Count || !newRoot.SequenceEqual(RootOrder, StringComparer.Ordinal))
        {
            RootOrder = newRoot;
            changed = true;
        }

        foreach (var folder in Folders)
        {
            var tok = FolderToken(folder.Id);
            if (seenRoot.Add(tok))
            {
                RootOrder.Add(tok);
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
            if (!string.IsNullOrWhiteSpace(app.Id) && placed.Add(app.Id))
            {
                RootOrder.Add(app.Id);
                changed = true;
            }
        }

        changed |= PruneInvalidDockItemsNonDestructive(folderIds);
        changed |= EnsurePositions(Appearance.Columns);
        return changed;
    }

    /// <summary>
    /// Explicitly cleans up a specific app from root, folders, positions, and dock after user uninstall.
    /// </summary>
    public void RemoveUninstalledApp(string appId)
    {
        if (string.IsNullOrWhiteSpace(appId)) return;

        bool changed = false;
        if (RootOrder.RemoveAll(t => string.Equals(t, appId, StringComparison.Ordinal)) > 0)
        {
            changed = true;
        }
        foreach (var folder in Folders)
        {
            if (folder.AppIds.RemoveAll(id => string.Equals(id, appId, StringComparison.Ordinal)) > 0)
            {
                changed = true;
            }
        }
        if (Positions.Remove(appId))
        {
            changed = true;
        }
        if (DockItems.RemoveAll(item =>
                string.Equals(item.Kind, "app", StringComparison.OrdinalIgnoreCase)
                && string.Equals(item.Target, appId, StringComparison.Ordinal)) > 0)
        {
            changed = true;
        }

        if (EnsurePositions(Appearance.Columns) || changed)
        {
            Save();
        }
    }

    public LauncherFolder? FindFolder(string folderId) =>
        Folders.FirstOrDefault(f => f.Id == folderId);

    public LauncherFolder CreateFolder(string name, string? initialAppId = null)
    {
        EnsurePositions(Appearance.Columns);

        LauncherGridPosition? initialPos = null;
        int insertIdx = -1;
        if (!string.IsNullOrWhiteSpace(initialAppId))
        {
            if (Positions.TryGetValue(initialAppId, out var savedPos))
            {
                initialPos = new LauncherGridPosition { Row = savedPos.Row, Col = savedPos.Col };
            }
            insertIdx = RootOrder.IndexOf(initialAppId);
        }

        var id = "f_" + Guid.NewGuid().ToString("N")[..8];
        var folder = new LauncherFolder
        {
            Id = id,
            Name = string.IsNullOrWhiteSpace(name) ? "新建文件夹" : name.Trim()
        };
        var folderToken = FolderToken(id);

        if (!string.IsNullOrWhiteSpace(initialAppId))
        {
            RemoveAppFromAllLocations(initialAppId);
            Positions.Remove(initialAppId);
            folder.AppIds.Add(initialAppId);
        }

        Folders.Add(folder);
        if (insertIdx >= 0 && insertIdx <= RootOrder.Count)
        {
            RootOrder.Insert(insertIdx, folderToken);
        }
        else
        {
            RootOrder.Add(folderToken);
        }

        if (initialPos is not null)
        {
            Positions[folderToken] = initialPos;
        }

        EnsurePositions(Appearance.Columns);
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

        EnsurePositions(Appearance.Columns);
        int columns = Math.Clamp(Appearance.Columns, 6, 12);
        var tok = FolderToken(folderId);

        LauncherGridPosition? folderPos = null;
        if (Positions.TryGetValue(tok, out var savedPos))
        {
            folderPos = new LauncherGridPosition { Row = savedPos.Row, Col = savedPos.Col };
        }

        var existingRootSet = new HashSet<string>(RootOrder, StringComparer.Ordinal);
        var unpackedApps = folder.AppIds
            .Where(id => !string.IsNullOrWhiteSpace(id) && !existingRootSet.Contains(id))
            .Distinct(StringComparer.Ordinal)
            .ToList();

        int idx = RootOrder.IndexOf(tok);
        if (idx >= 0)
        {
            RootOrder.RemoveAt(idx);
            RootOrder.InsertRange(idx, unpackedApps);
        }
        else
        {
            RootOrder.AddRange(unpackedApps);
        }

        Positions.Remove(tok);
        Folders.Remove(folder);
        DockItems.RemoveAll(item =>
            string.Equals(item.Kind, "folder", StringComparison.OrdinalIgnoreCase)
            && string.Equals(item.Target, folderId, StringComparison.Ordinal));

        var occupied = new HashSet<(int Row, int Col)>();
        foreach (var existingToken in RootOrder)
        {
            if (Positions.TryGetValue(existingToken, out var pos)
                && pos.Row >= 0
                && pos.Col >= 0
                && pos.Col < columns)
            {
                occupied.Add((pos.Row, pos.Col));
            }
        }

        int cursorRow = folderPos?.Row ?? 0;
        int cursorCol = folderPos is not null ? Math.Clamp(folderPos.Col, 0, columns - 1) : 0;
        foreach (var appId in unpackedApps)
        {
            var free = FindFirstFreeCellFrom(occupied, columns, cursorRow, cursorCol);
            Positions[appId] = new LauncherGridPosition { Row = free.Row, Col = free.Col };
            occupied.Add(free);
            cursorRow = free.Row;
            cursorCol = free.Col;
        }

        EnsurePositions(Appearance.Columns);
        Save();
    }

    public void MoveAppToFolder(string appId, string targetFolderId)
    {
        var target = FindFolder(targetFolderId);
        if (target is null) return;

        RemoveAppFromAllLocations(appId);
        Positions.Remove(appId);
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
        Positions.Remove(appId);
        RootOrder.Add(appId);
        EnsurePositions(Appearance.Columns);
        Save();
    }

    public void MoveAppToRootAt(string appId, int row, int col, bool repel = false)
    {
        if (string.IsNullOrWhiteSpace(appId)) return;

        EnsurePositions(Appearance.Columns);
        int columns = Math.Clamp(Appearance.Columns, 6, 12);
        row = Math.Max(0, row);
        col = Math.Clamp(col, 0, columns - 1);

        RemoveAppFromAllLocations(appId);
        Positions.Remove(appId);
        RootOrder.Add(appId);

        if (repel)
        {
            ApplyRepelPlacement(Positions, columns, appId, row, col);
        }
        else
        {
            string? occupant = FindTokenAt(row, col, excludeToken: appId);
            Positions[appId] = new LauncherGridPosition { Row = row, Col = col };
            if (occupant is not null)
            {
                var occupied = BuildOccupiedSet(columns, excludeToken: occupant);
                var free = FindFirstFreeCell(occupied, columns);
                Positions[occupant] = new LauncherGridPosition { Row = free.Row, Col = free.Col };
            }
        }

        EnsurePositions(columns);
        Save();
    }

    public LauncherFolder MergeAppsIntoNewFolder(string sourceAppId, string targetAppId, string folderName = "新建文件夹")
    {
        EnsurePositions(Appearance.Columns);

        LauncherGridPosition? targetPos = null;
        if (Positions.TryGetValue(targetAppId, out var savedTargetPos))
        {
            targetPos = new LauncherGridPosition { Row = savedTargetPos.Row, Col = savedTargetPos.Col };
        }
        int targetIdx = RootOrder.IndexOf(targetAppId);

        var id = "f_" + Guid.NewGuid().ToString("N")[..8];
        var targetFolder = new LauncherFolder
        {
            Id = id,
            Name = string.IsNullOrWhiteSpace(folderName) ? "新建文件夹" : folderName.Trim()
        };
        var folderToken = FolderToken(id);

        RemoveAppFromAllLocations(targetAppId);
        RemoveAppFromAllLocations(sourceAppId);
        Positions.Remove(targetAppId);
        Positions.Remove(sourceAppId);

        Folders.Add(targetFolder);
        if (targetIdx >= 0)
        {
            int insertIdx = Math.Clamp(targetIdx, 0, RootOrder.Count);
            RootOrder.Insert(insertIdx, folderToken);
        }
        else
        {
            RootOrder.Add(folderToken);
        }

        targetFolder.AppIds.Add(targetAppId);
        if (sourceAppId != targetAppId)
        {
            targetFolder.AppIds.Add(sourceAppId);
        }

        if (targetPos is not null)
        {
            Positions[folderToken] = targetPos;
        }

        EnsurePositions(Appearance.Columns);
        Save();
        return targetFolder;
    }

    public void AutoOrganize(IReadOnlyList<LinuxAppDto> apps)
    {
        if (apps.Count == 0) return;
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

    public void SetRootPosition(string token, int row, int col, bool repel = false)
    {
        if (!RootOrder.Contains(token, StringComparer.Ordinal)) return;
        EnsurePositions(Appearance.Columns);
        int columns = Math.Clamp(Appearance.Columns, 6, 12);

        row = Math.Max(0, row);
        col = Math.Clamp(col, 0, columns - 1);
        var old = Positions[token];
        if (old.Row == row && old.Col == col) return;

        if (repel)
        {
            ApplyRepelPlacement(Positions, columns, token, row, col);
        }
        else
        {
            string? occupant = FindTokenAt(row, col, excludeToken: token);
            Positions[token] = new LauncherGridPosition { Row = row, Col = col };
            if (occupant is not null)
            {
                Positions[occupant] = new LauncherGridPosition { Row = old.Row, Col = old.Col };
            }
        }
        Save();
    }

    /// <summary>
    /// Deterministic phone-style linear grid cell repulsion.
    /// Must stay identical to computeRepelPositions() in tray/webui/app.js.
    /// </summary>
    private static void ApplyRepelPlacement(
        Dictionary<string, LauncherGridPosition> positions,
        int columns,
        string dragToken,
        int targetRow,
        int targetCol)
    {
        int targetIdx = targetRow * columns + targetCol;
        int? sourceIdx = null;
        if (positions.TryGetValue(dragToken, out var oldPos))
        {
            sourceIdx = oldPos.Row * columns + oldPos.Col;
            if (sourceIdx.Value == targetIdx) return;
        }

        var occupiedByIdx = new Dictionary<int, string>(positions.Count);
        foreach (var pair in positions)
        {
            if (string.Equals(pair.Key, dragToken, StringComparison.Ordinal)) continue;
            int idx = pair.Value.Row * columns + pair.Value.Col;
            occupiedByIdx[idx] = pair.Key;
        }

        if (!occupiedByIdx.ContainsKey(targetIdx))
        {
            positions[dragToken] = new LauncherGridPosition { Row = targetRow, Col = targetCol };
            return;
        }

        int dir = (sourceIdx.HasValue && targetIdx > sourceIdx.Value) ? -1 : 1;
        int freeIdx = targetIdx + dir;
        while (freeIdx >= 0 && occupiedByIdx.ContainsKey(freeIdx))
        {
            freeIdx += dir;
        }

        if (freeIdx < 0)
        {
            dir = 1;
            freeIdx = targetIdx + 1;
            while (occupiedByIdx.ContainsKey(freeIdx))
            {
                freeIdx++;
            }
        }

        if (dir < 0)
        {
            for (int idx = freeIdx + 1; idx <= targetIdx; idx++)
            {
                if (!occupiedByIdx.TryGetValue(idx, out var tok)) continue;
                int nextIdx = idx - 1;
                positions[tok] = new LauncherGridPosition
                {
                    Row = nextIdx / columns,
                    Col = nextIdx % columns
                };
            }
        }
        else
        {
            for (int idx = freeIdx - 1; idx >= targetIdx; idx--)
            {
                if (!occupiedByIdx.TryGetValue(idx, out var tok)) continue;
                int nextIdx = idx + 1;
                positions[tok] = new LauncherGridPosition
                {
                    Row = nextIdx / columns,
                    Col = nextIdx % columns
                };
            }
        }

        positions[dragToken] = new LauncherGridPosition { Row = targetRow, Col = targetCol };
    }

    private string? FindTokenAt(int row, int col, string? excludeToken = null)
    {
        foreach (var pair in Positions)
        {
            if (excludeToken is not null && string.Equals(pair.Key, excludeToken, StringComparison.Ordinal))
            {
                continue;
            }
            if (pair.Value.Row == row && pair.Value.Col == col)
            {
                return pair.Key;
            }
        }
        return null;
    }

    private HashSet<(int Row, int Col)> BuildOccupiedSet(int columns, string? excludeToken = null)
    {
        var occupied = new HashSet<(int Row, int Col)>();
        foreach (var pair in Positions)
        {
            if (excludeToken is not null && string.Equals(pair.Key, excludeToken, StringComparison.Ordinal))
            {
                continue;
            }
            if (pair.Value.Row >= 0 && pair.Value.Col >= 0 && pair.Value.Col < columns)
            {
                occupied.Add((pair.Value.Row, pair.Value.Col));
            }
        }
        return occupied;
    }

    public void SetDockItems(
        IReadOnlyList<LauncherDockItem> items,
        IReadOnlyDictionary<string, LinuxAppDto> appLookup)
    {
        var folderIds = new HashSet<string>(Folders.Select(f => f.Id), StringComparer.Ordinal);
        var existingPinnedApps = new HashSet<string>(
            DockItems
                .Where(d => string.Equals(d.Kind, "app", StringComparison.OrdinalIgnoreCase) && !string.IsNullOrWhiteSpace(d.Target))
                .Select(d => d.Target),
            StringComparer.Ordinal);
        var seenTargets = new HashSet<string>(StringComparer.Ordinal);
        var seenIds = new HashSet<string>(StringComparer.Ordinal);
        var cleaned = new List<LauncherDockItem>();

        foreach (var source in items.Take(20))
        {
            var kind = (source.Kind ?? "").Trim().ToLowerInvariant();
            var target = (source.Target ?? "").Trim();
            if (!IsValidDockTarget(kind, target, appLookup, folderIds, existingPinnedApps)) continue;
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

    private bool PruneInvalidDockItemsNonDestructive(HashSet<string> folderIds)
    {
        var before = DockItems.Count;
        DockItems = DockItems
            .Where(item =>
            {
                var kind = (item.Kind ?? "").Trim().ToLowerInvariant();
                var target = (item.Target ?? "").Trim();
                if (kind == "app") return !string.IsNullOrWhiteSpace(target);
                if (kind == "folder") return folderIds.Contains(target);
                return IsValidSystemDockTarget(kind, target);
            })
            .ToList();
        return DockItems.Count != before;
    }

    private static bool IsValidSystemDockTarget(string kind, string target)
    {
        if (kind == "view" || kind == "action")
        {
            return AllowedDockTargets.Contains(target)
                   && target.StartsWith(kind + ":", StringComparison.Ordinal);
        }
        return false;
    }

    private static bool IsValidDockTarget(
        string kind,
        string target,
        IReadOnlyDictionary<string, LinuxAppDto> appLookup,
        HashSet<string> folderIds,
        HashSet<string> existingPinnedApps)
    {
        if (kind == "app")
        {
            return !string.IsNullOrWhiteSpace(target)
                   && (appLookup.ContainsKey(target) || existingPinnedApps.Contains(target));
        }
        if (kind == "folder") return folderIds.Contains(target);
        return IsValidSystemDockTarget(kind, target);
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
        => FindFirstFreeCellFrom(occupied, columns, 0, 0);

    private static (int Row, int Col) FindFirstFreeCellFrom(
        HashSet<(int Row, int Col)> occupied,
        int columns,
        int startRow,
        int startCol)
    {
        int row = Math.Max(0, startRow);
        int col = Math.Clamp(startCol, 0, columns - 1);
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
