# Rusplorer 🚀

A lightweight, blazingly-fast file explorer written in Rust for Windows.

## ⚡ Key Advantages

### Speed & Performance
- **Instant directory listing** - Displays folder contents immediately without waiting for file size calculations
- **Lazy loading** - File sizes load in the background while you browse
- **Background threading** - No UI blocking, ever smooth interaction
- **Minimal overhead** - Built with `egui` and `eframe` for efficient rendering

### Engineer-Friendly Design
- **Functionality first** - No bloat, no fancy animations, just pure functionality
- **Lightweight GUI** - Single executable, instant startup
- **Smart navigation** - Back/forward buttons with full history tracking
- **Multi-drive support** - Easy drive switching with visible drive selector

### User Experience
- **Mouse button support** - Use your mouse back/forward buttons for navigation
- **Keyboard shortcuts** - Alt+← and Alt+→ for back/forward
- **Visual organization** - Yellow-highlighted folders for quick identification
- **Two-column layout** - File names on the left, sizes aligned to the right
- **Scrollable view** - Handle thousands of files smoothly

## 📋 Features

- ✅ Fast directory browsing
- ✅ Support for all Windows drives
- ✅ Back/forward navigation with history
- ✅ Mouse button 4/5 support (side buttons)
- ✅ Keyboard navigation (Alt+arrows)
- ✅ Lazy-loaded file sizes
- ✅ Folder/file differentiation with colors
- ✅ Two-column layout (names + sizes)
- ✅ Scrollable file list
- ✅ No terminal window
- ✅ Pure GUI application

## 🛠️ Technical Stack

- **Language**: Rust
- **GUI Framework**: egui + eframe
- **Toolchain**: x86_64-pc-windows-gnu (MinGW)
- **Build Time**: ~18s (release)
- **Binary Size**: Compact standalone executable

## 🚀 Getting Started

### Build
```powershell
.\build.ps1
```

Or manually:
```powershell
cargo build --release
```

### Run
```powershell
.\target\release\rusplorer.exe
```

## 🔧 Development

We use a handy build script that:
1. Kills any running Rusplorer instance
2. Rebuilds the project
3. Launches the app

Just run `.\build.ps1` to quickly iterate!

## 📝 Roadmap

- [ ] Search functionality
- [ ] File preview
- [ ] Copy/paste operations
- [ ] Delete with recycle bin
- [ ] Drag and drop
- [ ] Custom themes
- [ ] File properties panel
- [ ] Recent locations

## 🎯 Philosophy

Rusplorer is built on the principle: **Speed and functionality over features and bling**. 

Every feature is evaluated based on:
- Does it improve responsiveness?
- Does it improve usability?
- Is it worth the complexity?

---

**Built by an engineer, for engineers.** ⚙️
