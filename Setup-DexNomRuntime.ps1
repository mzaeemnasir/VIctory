[CmdletBinding()]
param(
    [string] $GamePath,
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'DexNom Runtime'),
    [string] $ShortcutDirectory = [Environment]::GetFolderPath('Programs')
)

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT' -or ![Environment]::Is64BitOperatingSystem) {
    throw 'DexNom Runtime requires 64-bit Windows.'
}
if (!(Get-Command rustup -ErrorAction SilentlyContinue) -or !(Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Install Rust for Windows from https://rustup.rs, then reopen Setup.cmd.'
}

$InstallDir = [IO.Path]::GetFullPath($InstallDir)
for ($parent = [IO.DirectoryInfo]::new($InstallDir); $parent; $parent = $parent.Parent) {
    if (Test-Path -LiteralPath (Join-Path $parent.FullName '.victory-root')) {
        throw 'Choose an install directory outside the source checkout so the launcher can write saves and shader caches.'
    }
}
$config = Join-Path $InstallDir 'game-path.txt'
if (!$GamePath -and (Test-Path -LiteralPath $config -PathType Leaf)) {
    $saved = [IO.File]::ReadAllText($config).Trim()
    if ($saved -and (Test-Path -LiteralPath $saved -PathType Leaf)) { $GamePath = $saved }
}
if (!$GamePath) {
    Add-Type -AssemblyName System.Windows.Forms
    $picker = [Windows.Forms.OpenFileDialog]::new()
    try {
        $picker.Title = 'Select the Series X game executable from your extracted game installation'
        $picker.Filter = 'Game executable (*.exe)|*.exe'
        if ($picker.ShowDialog() -ne [Windows.Forms.DialogResult]::OK) {
            Write-Host 'Setup cancelled.'
            return
        }
        $GamePath = $picker.FileName
    } finally { $picker.Dispose() }
}
$game = Get-Item -LiteralPath $GamePath
if ($game.PSIsContainer -or $game.Extension -ine '.exe') {
    throw 'Select the game executable in its original installation directory.'
}

Push-Location -LiteralPath $PSScriptRoot
try {
    & (Join-Path $PSScriptRoot 'Initialize-FormatTables.ps1')
    $targetDir = Join-Path $PSScriptRoot 'target'
    $target = 'x86_64-pc-windows-msvc'
    & cargo build --locked --release -p victory --features diagnostics --bins --target $target --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) {
        throw 'Build failed. Check the compiler output and the C++ workload and Windows SDK listed in the README.'
    }
    $release = Join-Path $targetDir "$target/release"
    & (Join-Path $release 'victory.exe') inspect $game.FullName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'The runtime could not read the selected game executable.' }
} finally { Pop-Location }

# Hold the same lock as Play and Preload during an install or update.
$state = [IO.Directory]::CreateDirectory((Join-Path $InstallDir 'state'))
try {
    $lock = [IO.File]::Open((Join-Path $state.FullName 'launcher.lock'), 'OpenOrCreate', 'Write', 'None')
} catch [IO.IOException] {
    throw "Could not lock the install directory. Close DexNom Runtime and shader preload before updating. $($_.Exception.Message)"
}
try {
    foreach ($name in 'victory.exe','victory-play.exe','victory-warm.exe') {
        $destination = Join-Path $InstallDir $name
        $temp = Join-Path $InstallDir ([IO.Path]::GetRandomFileName())
        try {
            [IO.File]::Copy((Join-Path $release $name), $temp)
            if ([IO.File]::Exists($destination)) {
                [IO.File]::Replace($temp, $destination, [System.Management.Automation.Language.NullString]::Value)
            } else {
                [IO.File]::Move($temp, $destination)
            }
        } finally { if ([IO.File]::Exists($temp)) { [IO.File]::Delete($temp) } }
    }
    [IO.File]::WriteAllText($config, $game.FullName, [Text.UTF8Encoding]::new($false))
} finally { $lock.Dispose() }

$launcher = Join-Path $InstallDir 'victory-play.exe'
Write-Host "Installed DexNom Runtime in $InstallDir"
[IO.Directory]::CreateDirectory($ShortcutDirectory) | Out-Null
$shell = New-Object -ComObject WScript.Shell
foreach ($entry in @(
    @{ Name = 'DexNom Runtime'; Arguments = '' }
    @{ Name = 'DexNom Runtime (Experimental RT)'; Arguments = '--rt' }
    @{ Name = 'DexNom Runtime (Parallel CPU)'; Arguments = '--fast' }
)) {
    $shortcut = $shell.CreateShortcut((Join-Path $ShortcutDirectory ($entry.Name + '.lnk')))
    $shortcut.TargetPath = $launcher
    $shortcut.Arguments = $entry.Arguments
    $shortcut.WorkingDirectory = $InstallDir
    $shortcut.Save()
}
Write-Host "Open DexNom Runtime from $ShortcutDirectory to play, DexNom Runtime (Parallel CPU) to prepare frames on several CPU threads, or DexNom Runtime (Experimental RT) to try ray tracing."
