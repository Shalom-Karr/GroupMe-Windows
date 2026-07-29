<#
.SYNOPSIS
    Inspect the local GroupMe archive so you can tell "nothing synced yet" apart
    from "offline mode is broken".

.DESCRIPTION
    The offline reader renders whatever is in archive.db. If sync has not run
    long enough, an empty reader is correct behaviour that looks exactly like a
    bug. This reports what is actually stored.

    Run it BEFORE disconnecting. If message count is 0, offline mode has nothing
    to show and the test proves nothing.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\check_archive.ps1
    powershell -ExecutionPolicy Bypass -File tools\check_archive.ps1 -Watch
#>
[CmdletBinding()]
param(
    # Re-check every 15s so you can watch the backfill progress.
    [switch]$Watch
)

$AppDir = Join-Path $env:LOCALAPPDATA 'dev.shalomkarr.groupme'
$Db = Join-Path $AppDir 'archive.db'
$MediaDir = Join-Path $AppDir 'media'

function Format-Size([long]$bytes) {
    if ($bytes -ge 1GB) { '{0:N2} GB' -f ($bytes / 1GB) }
    elseif ($bytes -ge 1MB) { '{0:N1} MB' -f ($bytes / 1MB) }
    elseif ($bytes -ge 1KB) { '{0:N0} KB' -f ($bytes / 1KB) }
    else { "$bytes B" }
}

function Show-Report {
    Write-Host ''
    Write-Host '  GroupMe archive' -ForegroundColor Cyan
    Write-Host "  $AppDir" -ForegroundColor DarkGray
    Write-Host ''

    if (-not (Test-Path $AppDir)) {
        Write-Host '  Nothing here yet.' -ForegroundColor Yellow
        Write-Host '  The app has not run, or has not signed in.'
        return $false
    }

    if (-not (Test-Path $Db)) {
        Write-Host '  archive.db missing' -ForegroundColor Yellow
        Write-Host '  The app created its folder but never opened a database.'
        return $false
    }

    $db = Get-Item $Db
    Write-Host ("  archive.db        {0,10}   modified {1}" -f (Format-Size $db.Length), $db.LastWriteTime.ToString('HH:mm:ss'))

    # WAL holds writes not yet folded into the main file. A large WAL alongside a
    # small archive.db means sync IS working -- the data just has not checkpointed.
    $wal = Join-Path $AppDir 'archive.db-wal'
    if (Test-Path $wal) {
        $w = Get-Item $wal
        Write-Host ("  archive.db-wal    {0,10}   (uncheckpointed writes)" -f (Format-Size $w.Length)) -ForegroundColor DarkGray
    }

    if (Test-Path $MediaDir) {
        $media = Get-ChildItem $MediaDir -File -ErrorAction SilentlyContinue
        $mediaBytes = ($media | Measure-Object -Property Length -Sum).Sum
        Write-Host ("  media\            {0,10}   {1} files" -f (Format-Size ([long]$mediaBytes)), $media.Count)
    } else {
        Write-Host '  media\            (none yet)' -ForegroundColor DarkGray
    }

    # Row counts need sqlite3; it is not part of Windows. Absence is not a
    # failure -- the file sizes above already answer "is anything syncing".
    $sqlite = Get-Command sqlite3 -ErrorAction SilentlyContinue
    if ($sqlite) {
        Write-Host ''
        $q = @"
SELECT 'conversations', COUNT(*) FROM conversations
UNION ALL SELECT 'messages', COUNT(*) FROM messages
UNION ALL SELECT 'users', COUNT(*) FROM users
UNION ALL SELECT 'cached media', COUNT(*) FROM media_cache
UNION ALL SELECT 'backfills done', COUNT(*) FROM sync_state WHERE backfill_complete = 1;
"@
        # Read-only URI so this can never disturb a running app.
        $rows = & sqlite3 "file:$Db?mode=ro" -separator '|' $q 2>$null
        if ($LASTEXITCODE -eq 0 -and $rows) {
            foreach ($r in $rows) {
                $parts = $r -split '\|'
                Write-Host ("  {0,-16} {1,10:N0}" -f $parts[0], [int]$parts[1])
            }
            $msgCount = (($rows | Where-Object { $_ -like 'messages|*' }) -split '\|')[1]
            Write-Host ''
            if ([int]$msgCount -gt 0) {
                Write-Host '  Ready to test offline.' -ForegroundColor Green
                Write-Host '  Disconnect now; expect the window to swap in ~10-15s.'
            } else {
                Write-Host '  No messages archived yet.' -ForegroundColor Yellow
                Write-Host '  Sign in and leave the app running a few minutes first --'
                Write-Host '  going offline now would show an empty reader, which proves nothing.'
            }
        }
    } else {
        Write-Host ''
        Write-Host '  (install sqlite3 for row counts; file sizes above are enough' -ForegroundColor DarkGray
        Write-Host '   to tell whether anything is being written)' -ForegroundColor DarkGray
        if ($db.Length -gt 100KB) {
            Write-Host ''
            Write-Host '  Database has real content. Ready to test offline.' -ForegroundColor Green
        }
    }

    # Logs are where a failed token capture or a rejected sync shows up.
    $logDir = Join-Path $AppDir 'logs'
    if (Test-Path $logDir) {
        $log = Get-ChildItem $logDir -Filter *.log -ErrorAction SilentlyContinue |
               Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($log) {
            Write-Host ''
            Write-Host "  latest log: $($log.FullName)" -ForegroundColor DarkGray
            Get-Content $log.FullName -Tail 6 -ErrorAction SilentlyContinue |
                ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
        }
    }
    return $true
}

if ($Watch) {
    Write-Host 'Watching every 15s. Ctrl-C to stop.' -ForegroundColor DarkGray
    while ($true) { Clear-Host; [void](Show-Report); Start-Sleep -Seconds 15 }
} else {
    [void](Show-Report)
    Write-Host ''
}
