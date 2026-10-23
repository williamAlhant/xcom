xcom renders remote X11 windows as native windows. SSH is used only for the
bootstrap; pixels and input travel over a pinned TLS 1.3 channel.

| file | runs on |
|---|---|
| `xcomd-linux-x86_64` | remote Linux host (static; needs `xvfb`) |
| `xcom-linux-x86_64` | Linux client (glibc; uses the desktop's X11/Wayland libs) |
| `xcom-windows-x86_64.exe` | Windows x86_64 client |

Quick start: `xcom connect --ssh user@host --exec 'xterm'`

Notes: the Windows binary is unsigned (SmartScreen warning); SSH is bootstrap-only;
not security-audited; developed with extensive AI assistance. See the README for
limitations.
