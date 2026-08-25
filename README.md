# DeepSeek Harness Desktop

把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)(dsh) Web 界面封装为原生 **Windows 桌面应用**的 Tauri v2 外壳。

## 截图

![对话](docs/screenshots/chat.png)

![harness](docs/screenshots/harness.png)

![更多](docs/screenshots/more.png)

## 特色

- **原生桌面体验**:基于 Tauri v2(WebView2)，无浏览器地址栏、无系统菜单干扰。
- **自动启动 dsh 服务**:启动时自动执行 `npx @deepseek-ai/dsh web --port 3080`(可用 `DSH_PORT` 环境变量改端口)，加载完成后自动导航到 Harness 界面。
- **一键环境自检与自动安装**:启动时自动检测运行环境(WebView2 / Node.js / dsh)，缺失的组件会**自动下载安装**(WebView2 用官方引导器静默安装、Node.js 依次尝试 winget / 官方 MSI / 免管理员便携版、dsh 用 `npm install -g`)，安装过程、下载百分比与明细日志都会在启动页面实时展示;安装完成后自动拉起服务。若自动修复失败，页面会给出具体原因、手动指引与“重试”按钮。
- **内置 DeepSeek Chat**:标题栏一键在 Harness 与官方 DeepSeek Chat 网页之间切换。
- **更新能力**:“关于”中可检查更新并一键更新到最新版(从 GitHub Release 下载便携 exe 自动替换并重启)。
- **单实例**:重复启动会聚焦已有窗口。
- **全局快捷键**:`F12` 打开/关闭当前页面的 DevTools。

## 运行

### 前置条件

| 依赖 | 说明 |
|---|---|
| Windows 10/11 | 支持 x64 |
| [Node.js](https://nodejs.org/) ≥ 20 | 运行 dsh 服务所需 |
| [@deepseek-ai/dsh](https://www.npmjs.com/package/@deepseek-ai/dsh) | 全局安装:`npm install -g @deepseek-ai/dsh` |
| WebView2 | Windows 10/11 系统自带，无需额外安装 |

> 以上依赖缺失时，应用会在启动页面自动检测并一键安装，无需手动操作(WebView2 缺失时通过原生对话框引导安装，因为缺失它时页面无法渲染)。

### 启动

- 方式一:直接运行[release/deepseek-harness.exe
](https://github.com/wq3333/deepseek-harness-desktop/releases/latest)
- 方式二:开发运行 `cd src && npm run tauri dev`

### 标题栏功能介绍

| 区域 | 功能 |
|---|---|
| Chat | 切换到官方 DeepSeek Chat 网页 |
| Harness | 切换到 dsh Harness 界面(默认) |
| 设置 | 自动关闭dsh和更新 |

## 构建

### 前置条件

| 依赖 | 说明 |
|---|---|
| [Node.js](https://nodejs.org/) ≥ 20 | 含 npm |
| [Rust](https://rustup.rs/)(MSVC toolchain) | `rustup default stable-x86_64-pc-windows-msvc` |

> 本项目**只产出便携 exe，不生成安装包**。WebView2 运行时为系统自带，无需打包。

### 构建脚本调用

一键构建(推荐，与 CI 一致):

```bat
src\publish.bat
```

执行后会把便携版 `deepseek-harness.exe` 复制到`publish\DeepSeekHarness.exe`，直接双击即可运行。

等价的手工命令:

```bash
cd src
npm install
npm run tauri build -- --no-bundle   # 产出 src\src-tauri\target\release\deepseek-harness.exe
```

## 插件推荐

dsh 通过 profile 管理插件，在 dsh web profile 中安装:

```bash
dsh plugin --profile web add <包名>
```

| 插件 | 说明 |
|---|---|
| [dshmarket](https://github.com/dsh-market/dsh-market) | dsh 内置的可视化插件市场:浏览、搜索、一键安装社区插件。 |
| [dsh-liquid-glass](https://github.com/xingyingyuzhui/dsh-liquid-glass) | 为 Harness 添加壁纸与“液态玻璃”(Liquid Glass)叠加视觉效果，兼容官方浅色 / 深色 / 跟随系统主题。 |
| [dsh-better-sidebar](https://github.com/omdsh-dev/DSH-better-sidebar) | VSCode 风格的右侧栏(资源管理器 / 编辑器 / 终端 / Git / 浏览器)，按会话隔离，并暴露服务供其他插件注册侧栏页与文件查看器。 |

## License

[MIT](LICENSE)
