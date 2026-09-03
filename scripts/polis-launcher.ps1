<#
  Polis launcher - the friendly front door.

  Nobody should need to know a command line to see their agents as a city. This
  finds the binary (offering to build it), then offers everything Polis can do.
  A folder dragged onto Polis.bat arrives as -Repo and skips the repository
  menu.

  Run it by double-clicking Polis.bat in the repo root.
#>

param(
  # A folder dragged onto Polis.bat arrives here and skips the repository menu.
  [string]$Repo,
  # Skip the action menu and do this. Used by nothing yet; handy for a shortcut.
  [ValidateSet('watch', 'map', 'run', 'picture', 'doctor')]
  [string]$Do
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Exe  = Join-Path $Root 'target\release\polis.exe'

# polis.exe pauses on failure when it was started with no arguments and has a
# terminal, so that a double-clicked binary never vanishes. Here the launcher
# does its own pause, and two of them in a row is one too many.
$env:POLIS_NO_PAUSE = '1'

function Say([string]$t, [string]$c = 'Gray') { Write-Host $t -ForegroundColor $c }
function Title([string]$t) {
  Write-Host ''
  Write-Host "  $t" -ForegroundColor White
  Write-Host "  $('-' * $t.Length)" -ForegroundColor DarkGray
}
function Pause-Exit([int]$code = 0) {
  # The pause exists so a double-clicked window does not vanish before it can be
  # read. With no human at a console (CI, a pipe) it must neither block nor
  # change the exit code, so skip it entirely rather than trying to read.
  if ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected) {
    Write-Host ''
    Write-Host '  Press any key to close...' -ForegroundColor DarkGray
    try { $null = $Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown') } catch { }
  }
  exit $code
}

Clear-Host
Write-Host ''
Write-Host '   P O L I S' -ForegroundColor Cyan
Write-Host '   your coding agents, drawn as a city seen from above' -ForegroundColor DarkGray

# ---------------------------------------------------------------- the binary

if (-not (Test-Path $Exe)) {
  Title 'First run'
  Say '  Polis has not been built yet. That takes a few minutes the first time.'
  Say ''
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Say '  Rust is not installed, so it cannot be built here.' 'Yellow'
    Say '  Install Rust from https://rustup.rs and run this again.'
    Pause-Exit 1
  }
  $go = Read-Host '  Build it now? [Y/n]'
  if ($go -and $go -notmatch '^[Yy]') { Pause-Exit 0 }
  Say ''
  Say '  Building (this is the slow part, once)...' 'DarkGray'
  Push-Location $Root
  try {
    & cargo build --release -p polis-app
    # The hook transport is a separate, tiny binary with its own profile. Only
    # `polis connect` needs it, and building it now means that path is never a
    # second wait.
    & cargo build --profile hook -p polis-hook
  } finally { Pop-Location }
  if (-not (Test-Path $Exe)) { Say '  Build failed. The output above says why.' 'Red'; Pause-Exit 1 }
}

# ------------------------------------------------------------------ on PATH?

# The first command in the getting-started guide is `polis`, and a fresh
# checkout has it nowhere near PATH. Offering it here - once, with consent, at
# user scope - is the difference between "it works" and "now go and edit an
# environment variable". Never `setx PATH "%PATH%;..."`: %PATH% there is the
# combined machine and user value, so that line copies the system path into the
# user one, permanently, truncated at 1024 characters.

$BinDir     = Split-Path $Exe -Parent
$ConfigDir  = Join-Path $env:LOCALAPPDATA 'polis'
$PathMarker = Join-Path $ConfigDir 'path-offer'

$OnPath = $false
try {
  $found = Get-Command polis -ErrorAction SilentlyContinue
  if ($found) { $OnPath = $true }
} catch { }

if (-not $OnPath -and -not (Test-Path $PathMarker)) {
  Title 'Type "polis" from anywhere?'
  Say ''
  Say '  Polis is built, but the folder it is in is not on your PATH, so the'
  Say '  command `polis` only works spelled out in full. This adds'
  Say ''
  Say "    $BinDir" 'White'
  Say ''
  Say '  to your own user PATH - not the machine''s - and changes nothing else.'
  Say '  It applies to terminals you open after this. Answered once either way.'
  Say ''
  $addIt = Read-Host '  Add it? [Y/n]'
  if (-not $addIt -or $addIt -match '^[Yy]') {
    try {
      $current = [Environment]::GetEnvironmentVariable('Path', 'User')
      $already = $false
      if ($current) {
        foreach ($entry in $current.Split(';')) {
          if ($entry.Trim().TrimEnd('\') -eq $BinDir.TrimEnd('\')) { $already = $true }
        }
      }
      if (-not $already) {
        $joined = if ($current) { "$current;$BinDir" } else { $BinDir }
        [Environment]::SetEnvironmentVariable('Path', $joined, 'User')
      }
      # This window too, so the rest of this session can say `polis`.
      $env:PATH = "$BinDir;$env:PATH"
      Say ''
      Say '  Done. In a new terminal, `polis` now works from any folder.' 'Green'
      Say '  To undo it: Settings -> Environment Variables -> Path (User).' 'DarkGray'
    } catch {
      Say ''
      Say "  Could not write it: $($_.Exception.Message)" 'Yellow'
      Say '  Everything below still works - this only affects typing `polis`.'
    }
  } else {
    Say ''
    Say '  Left alone. Run Polis from this file, or spell the path out:' 'DarkGray'
    Say "    $Exe" 'DarkGray'
  }
  try {
    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    Set-Content -LiteralPath $PathMarker -Value 'asked' -Encoding utf8
  } catch { }
}

# ------------------------------------------------------------ what to do

# How many sessions are already on this machine, so the menu can say. This is
# the headers-only scan polis itself uses; it costs tens of milliseconds.
$Sessions = 0
try {
  $projects = Join-Path $env:USERPROFILE '.claude\projects'
  if (Test-Path $projects) {
    $Sessions = @(Get-ChildItem -LiteralPath $projects -Recurse -Filter '*.jsonl' -File -Depth 1 -ErrorAction SilentlyContinue).Count
  }
} catch { }

$action = $Do
if (-not $action) {
  Title 'What would you like to do?'
  Write-Host ''
  $seen = if ($Sessions -gt 0) { "$Sessions found on this machine" } else { 'none found yet' }
  Write-Host "   1. Watch a past session      replay something you already ran ($seen)" -ForegroundColor Gray
  Write-Host '   2. Map a repository          open the city window for a checkout' -ForegroundColor Gray
  Write-Host '   3. Connect a live agent      start Claude Code with the map watching' -ForegroundColor Gray
  Write-Host '   4. Save a picture            write the city plan to a PNG on the Desktop' -ForegroundColor Gray
  Write-Host '   5. Check my setup            what is wrong, and how to fix it' -ForegroundColor Gray
  Write-Host ''
  Say '  In the city: every building is a file, every district a folder, and the' 'DarkGray'
  Say '  tallest building is the file with the most uncommitted work - so the' 'DarkGray'
  Say '  skyline points at whatever most needs reviewing.' 'DarkGray'
  Write-Host ''
  if ($Repo) { Say "  A folder was dragged on: $Repo" 'DarkGray'; Write-Host '' }
  $pick = Read-Host '  Choose [1]'
  if (-not $pick) { $pick = '1' }
  switch ($pick.Trim()) {
    '1' { $action = 'watch' }
    '2' { $action = 'map' }
    '3' { $action = 'run' }
    '4' { $action = 'picture' }
    '5' { $action = 'doctor' }
    default { Say '  Nothing chosen.' 'Yellow'; Pause-Exit 0 }
  }
}

# `watch` and `doctor` need no repository at all, so they run before the finder.

if ($action -eq 'watch') {
  Title 'Your past sessions'
  Write-Host ''
  Say '  A window is opening with every session recorded on this machine, most' 'DarkGray'
  Say '  recent first. Pick one and it replays over that repository''s own city.' 'DarkGray'
  Say '  Space pauses, . and , step, h shows every key. It is a recording;' 'DarkGray'
  Say '  nothing you do in there can break anything.' 'DarkGray'
  Write-Host ''
  & $Exe watch
  if ($LASTEXITCODE -ne 0) { Say ''; Say '  That did not work. The output above says why.' 'Red'; Pause-Exit 1 }
  Pause-Exit 0
}

if ($action -eq 'doctor') {
  & $Exe doctor
  Write-Host ''
  Say '  Anything above that is not "ok" has the command that fixes it beside it.' 'DarkGray'
  Say '  Some of them Polis can do itself:  polis doctor --fix' 'DarkGray'
  Pause-Exit 0
}

# ------------------------------------------- a folder was dragged on: skip the menu

if ($Repo) {
  $Repo = $Repo.Trim('"', ' ')
  if (-not (Test-Path (Join-Path $Repo '.git'))) {
    Title 'Not a git repository'
    Say "  $Repo" 'Yellow'
    Say ''
    Say '  Polis builds the city from git history, so it needs a folder with a'
    Say '  .git directory in it. Try dragging a different folder on, or just'
    Say '  double-click Polis.bat to pick from a list.'
    Pause-Exit 1
  }
  $chosen = (Resolve-Path -LiteralPath $Repo).Path
}

# ------------------------------------------------------- find git repositories

if (-not $chosen) {

# One folder that contains your repositories - asked once, then remembered.
# Deliberately not a scan of the whole drive: it is slow, it surprises people,
# and everyone already keeps their code in one place.

$ConfigDir  = Join-Path $env:LOCALAPPDATA 'polis'
$ConfigFile = Join-Path $ConfigDir 'code-directory.txt'

$CodeDir = $null
if (Test-Path $ConfigFile) {
  $saved = (Get-Content -LiteralPath $ConfigFile -Raw -ErrorAction SilentlyContinue).Trim()
  if ($saved -and (Test-Path $saved)) { $CodeDir = $saved }
}

if (-not $CodeDir) {
  $guess = Split-Path $Root -Parent
  Title 'Where do you keep your code?'
  Say ''
  Say '  Give the folder your repositories live in - the one that contains them,'
  Say '  not a repository itself. Asked once, then remembered.'
  Say ''
  Say "  Press Enter for: $guess" 'DarkGray'
  Say ''
  $typed = (Read-Host '  Folder').Trim('"', ' ')
  if (-not $typed) { $typed = $guess }
  if (-not (Test-Path $typed)) { Say ''; Say "  No such folder: $typed" 'Yellow'; Pause-Exit 1 }
  $CodeDir = (Resolve-Path -LiteralPath $typed).Path
  try {
    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    Set-Content -LiteralPath $ConfigFile -Value $CodeDir -Encoding utf8
    Say ''
    Say "  Remembered. To change it later, delete:" 'DarkGray'
    Say "    $ConfigFile" 'DarkGray'
  } catch { }
}

Title "Repositories in $CodeDir"

$repos = [System.Collections.Generic.List[object]]::new()

function Add-Repo([string]$path) {
  if (-not $path) { return }
  try { $full = (Resolve-Path -LiteralPath $path -ErrorAction Stop).Path } catch { return }
  if (-not (Test-Path (Join-Path $full '.git'))) { return }
  # git ls-files is far faster than walking the tree and it already honours
  # .gitignore, so the count matches what Polis will actually map.
  $files = 0
  try {
    $out = & git -C $full ls-files 2>$null
    if ($LASTEXITCODE -eq 0 -and $out) { $files = @($out).Count }
  } catch { }
  $repos.Add([pscustomobject]@{ Path = $full; Name = Split-Path $full -Leaf; Files = $files })
}

try {
  Get-ChildItem -LiteralPath $CodeDir -Directory -ErrorAction SilentlyContinue |
    ForEach-Object { Add-Repo $_.FullName }
} catch { }
# The folder itself may be a repository.
Add-Repo $CodeDir

if ($repos.Count -eq 0) {
  Say ''
  Say '  No git repositories directly inside that folder.' 'Yellow'
  Say '  You can still type a path below.'
} else {
  Say "  Found $($repos.Count)." 'DarkGray'
}

# --------------------------------------------------------------- pick one

$ordered = @($repos | Sort-Object -Property @{Expression='Files';Descending=$true})

Title 'Which repository?'
Write-Host ''
$max = [Math]::Min($ordered.Count, 12)
for ($i = 0; $i -lt $max; $i++) {
  $r = $ordered[$i]
  $n = ('{0,2}' -f ($i + 1))
  $size = if ($r.Files -ge 1000) { 'a city' } elseif ($r.Files -ge 200) { 'a town' } else { 'a village' }
  Write-Host ("   {0}. {1,-28} {2,6} files  ({3})" -f $n, $r.Name, $r.Files, $size) -ForegroundColor Gray
  Write-Host ("       {0}" -f $r.Path) -ForegroundColor DarkGray
}
Write-Host ''
Write-Host '    P. type a path myself' -ForegroundColor DarkGray
Write-Host ''
Say '  Bigger repositories with years of history make better cities -' 'DarkGray'
Say '  the old, dense core is literally the code you wrote first.' 'DarkGray'
Write-Host ''

$choice = Read-Host '  Choose'
if ($choice -match '^[Pp]') {
  $typed = Read-Host '  Path to a git repository'
  $typed = $typed.Trim('"', ' ')
  if (Test-Path (Join-Path $typed '.git')) { $chosen = (Resolve-Path -LiteralPath $typed).Path }
  else { Say "  That is not a git repository: $typed" 'Yellow'; Pause-Exit 1 }
} elseif ($choice -match '^\d+$' -and [int]$choice -ge 1 -and [int]$choice -le $max) {
  $chosen = $ordered[[int]$choice - 1].Path
} else {
  Say '  Nothing chosen.' 'Yellow'; Pause-Exit 0
}

}  # end of the repository menu

$name = Split-Path $chosen -Leaf

# ----------------------------------------------------------------- do it

if ($action -eq 'map') {
  Title "Mapping $name"
  Write-Host ''
  Say '  A window is opening. Drag to pan, scroll to zoom, click a building to' 'DarkGray'
  Say '  open that file. Close the window to come back here.' 'DarkGray'
  Write-Host ''
  & $Exe --repo $chosen map
  if ($LASTEXITCODE -ne 0) { Say ''; Say '  That did not work. The output above says why.' 'Red'; Pause-Exit 1 }
  Pause-Exit 0
}

if ($action -eq 'run') {
  Title "Starting an agent in $name"
  Write-Host ''
  Say '  Claude Code is about to start in this window, exactly as if you had' 'DarkGray'
  Say '  typed `claude` yourself - same prompt, same keys. Polis sets up what it' 'DarkGray'
  Say '  needs on that one process; nothing is changed in your shell.' 'DarkGray'
  Say '  The map opens in its own window beside it. Today it shows the city and' 'DarkGray'
  Say '  the session plays back afterwards - Polis prints the one command for' 'DarkGray'
  Say '  that when the agent exits.' 'DarkGray'
  Write-Host ''
  # The launcher's pause would sit on top of the agent's own screen, so this one
  # path lets polis print its own closing summary and returns.
  & $Exe --repo $chosen run -- claude
  Pause-Exit $LASTEXITCODE
}

# picture

$out  = Join-Path ([Environment]::GetFolderPath('Desktop')) "polis-$name.png"
$junc = Join-Path ([Environment]::GetFolderPath('Desktop')) "polis-$name-junctions.png"

Title "Drawing $name"
Write-Host ''

& $Exe -C $chosen snapshot --out $out --junctions $junc
if ($LASTEXITCODE -ne 0) { Say ''; Say '  That did not work. The output above says why.' 'Red'; Pause-Exit 1 }

Title 'Done'
Say "  City plan     $out"
Say "  Road graph    $junc"
Write-Host ''
Say '  Opening the city plan now.' 'DarkGray'
Say '  In it: every building is a file, every district a directory, and the' 'DarkGray'
Say '  tallest building is the file with the most uncommitted work - so the' 'DarkGray'
Say '  skyline points at whatever most needs reviewing.' 'DarkGray'
Write-Host ''
Say '  The second image is the road network alone, with junctions coloured by' 'DarkGray'
Say '  how many streets meet there. It is how you can tell the town grew' 'DarkGray'
Say '  rather than being drawn.' 'DarkGray'

Start-Process $out
Pause-Exit 0
