#ifndef AppVersion
  #define AppVersion "2.1.0"
#endif
[Setup]
AppId={{B7C0F3A2-5D1E-4C8B-9A64-2F1E7D3A9C10}
AppName=Job Finder
AppVersion={#AppVersion}
AppPublisher=Job Finder
DefaultDirName={autopf}\JobFinder
DefaultGroupName=Job Finder
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=JobFinder-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\job-finder.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "..\target\release\job-finder.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Job Finder"; Filename: "{app}\job-finder.exe"; IconFilename: "{app}\job-finder.exe"
Name: "{autodesktop}\Job Finder"; Filename: "{app}\job-finder.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Put a Job Finder icon on the desktop"

[Run]
Filename: "{app}\job-finder.exe"; Description: "Start Job Finder"; Flags: nowait postinstall skipifsilent
