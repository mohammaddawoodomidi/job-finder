# Job Finder 2.1.0 (Rust)
Finds entry-level / no-experience jobs from websites (RSS, JSON-LD, HTML, JSON API) and public Telegram channels.
- Run: `cargo run --release` (Windows opens a native window; elsewhere your browser). `--browser` forces the browser, `--selftest`, `--scan [days]` run headless.
- Data (settings, output, cache, logs): `JOBFINDER_HOME`, else next to the exe if writable, else `%LOCALAPPDATA%\JobFinder`.
- Not ported: Telethon mode (falls back to web preview) and the offline Argos translator (use libretranslate or none).
- Installer: see `.github/workflows/build.yml` + `installer/JobFinder.iss`.
