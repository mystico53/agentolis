<#
  Polis launcher - the friendly front door.

  Nobody should need to know a command line to see their repository as a city.
  This finds the binary (offering to build it), finds git repositories on this
  machine, and renders the one you pick.

  Run it by double-clicking Polis.bat in the repo root.
#>

param(
  # A folder dragged onto Polis.bat arrives here and skips the menu.
  [string]$Repo
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Exe  = Join-Path $Root 'target\release\polis.exe'

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
Write-Host '   your repository, drawn as a city seen from above' -ForegroundColor DarkGray

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
  try { & cargo build --release -p polis-app } finally { Pop-Location }
  if (-not (Test-Path $Exe)) { Say '  Build failed. The output above says why.' 'Red'; Pause-Exit 1 }
}

# --------------------------------------- a folder was dragged on: skip the menu

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

Title 'Which repository should Polis map?'
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

}  # end of the menu path

# ------------------------------------------------------------------- render

$name = Split-Path $chosen -Leaf
$out  = Join-Path ([Environment]::GetFolderPath('Desktop')) "polis-$name.png"
$junc = Join-Path ([Environment]::GetFolderPath('Desktop')) "polis-$name-junctions.png"

Title "Mapping $name"
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
