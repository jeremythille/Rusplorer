# Rusplorer 🚀

A lightweight, blazingly-fast file explorer written in Rust for Windows.

<div align="center">

![Rusplorer logo](./source_code/logo/rusplorer_logo_128.png)

</div>

## Disclaimer

I custom-made this file explorer specifically for my needs, as I like it. That's why there are next to no options or configuration. Consequently, you may like it too, or you may not.  
**It is not intended to be pretty, but fast as hell, minimalist, and practical.** I couldn't care less about it being pretty.

<div align="center">

<br>

![Rusplorer screenshot](./source_code/screenshot.png)

</div>

## ⚡ Key Advantages

### Speed & Performance
- **Super lightweight** - 7 MB self-contained executable, 2 MB of which are due to embedded font (Iosevka Aile)
- **Instant directory listing** - Displays folder contents immediately without waiting for file size calculations
- **Lazy loading** - File sizes load in the background while you browse
- **Background threading** - No UI blocking, ever smooth interaction
- **Minimal overhead** - Built with `egui` and `eframe` for efficient rendering

### Engineer-Friendly Design
- **Functionality first** - No bloat, no fancy animations, just pure functionality
- **Lightweight GUI** - Single executable, instant startup
- **Multi-drive support** - Easy drive switching with visible drive selector
- **Possibility to save state as .rsess files**

### User Experience
- **Tabs support**
- **Side mouse button support** - Use your mouse back/forward buttons for navigation
- **2.5 column layout** - File names on the left, sizes aligned to the right, with optional modification date column
- **Interactive breadcrumb**

## 🚀 How to run the program

No installation needed.  
There's a single executable in the root folder. Just launch rusplorer.exe, that's it.


## 📋 Features

- ✅ Fast directory browsing
- ✅ View all folders and file sizes (lazy-loaded)
- ✅ Support for all Windows drives
- ✅ Back/forward navigation with history
- ✅ Mouse button 4/5 support (side buttons)
- ✅ Keyboard navigation (Alt+arrows)
- ✅ Folder/file differentiation with colors
- ✅ 2.5-column layout (names + sizes + optional date)
- ✅ GUI application, no terminal window

## 🛠️ Technical Stack

- **Language**: Rust
- **GUI Framework**: egui + eframe
- **Toolchain**: x86_64-pc-windows-gnu (MinGW)
- **Build Time**: ~18s (release)
- **Binary Size**: Compact standalone executable

## Optional (for developers): how to build the app


I use a handy build script that:
1. Kills any running Rusplorer instance
2. Rebuilds the project
3. Launches the app

```powershell
./source_code/build.ps1
```

Or manually:
```powershell
cargo build --release
```
