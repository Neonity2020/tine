<p align="center">
  <img src="docs/logo.svg" alt="Tine" height="84">
</p>

<p align="center">
  <b>流畅、本地运行、兼容 Logseq 的大纲笔记软件。</b><br>
  可直接使用与 Logseq <i>相同的</i> Markdown 图谱——可以在同一组文件上交替使用两款应用。
</p>

<p align="center">
  <a href="README.md">English</a> | <a href="README.zh-CN.md">简体中文</a><br>
  <sub>本译文最初基于英文 README 修订版 <code>682cba63acb0ffb62698f0a063949f8e2515a230</code>（2026-07-10）。若中英文内容存在差异，请以最新英文 README 为准。</sub>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white" alt="Tauri 2">
  <img src="https://img.shields.io/badge/SolidJS-1.9-2C4F7C?logo=solid&logoColor=white" alt="SolidJS">
  <img src="https://img.shields.io/badge/Rust-2021-000000?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows%20%7C%20Android%20%7C%20iOS-555" alt="Platforms">
  <img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="AGPL-3.0">
</p>

<p align="center">
  <img src="docs/img/hero.png" alt="Tine — 日记视图" width="820">
</p>

<p align="center">
  <b>▶ <a href="https://tine.page/guide/">在浏览器中在线试用</a></b> —— 使用 Tine 自身的实时导出功能发布的使用指南：搜索、标签页和分屏都可以直接使用。<br>
  <a href="https://tine.page/">网站</a> · <a href="https://tine.page/compare.html">Tine 与 Logseq 对比</a> · <a href="https://github.com/martinkoutecky/tine/discussions">Discussions</a> · <a href="#support">♥ 支持</a>
</p>

---

## 什么是 Tine？

Tine 是一款适用于桌面端和移动端的大纲笔记软件，外观和操作体验与 [Logseq](https://logseq.com) 相近，但它要流畅很多。它直接使用标准的 Logseq 图谱目录结构——
`journals/`、`pages/`、`assets/` 和 `logseq/config.edn`——因此可以用它打开你的笔记图谱，并在两款应用之间交替编辑（同一时间只运行一个）。文件会以兼容 Logseq 的 Markdown 格式写回，因此在两款软件之间**无需导入或导出，也不会被锁定在某个应用或专有格式中**。

**为什么要开发 Tine？** Logseq 的界面基于 Electron 和 DataScript，在处理大型图谱时，较重的重复渲染容易让操作逐渐变慢。Tine 因此选择从底层重新实现：使用轻量的原生应用外壳 Tauri/WebKitGTK，以纯 Rust 核心负责解析和索引，并采用 SolidJS 构建细粒度响应式前端，无需反复比较和更新虚拟 DOM。编辑器会在前端直接维护实时块树，因此每次按键都不必在前端与 Rust 核心之间来回传递；搜索、反向链接和查询等需要读取整个图谱的操作，则直接使用内存缓存，而不必每次重新解析文件。

> **状态：** 已可在 Linux、macOS、Windows 和 Android 上作为日常主力工具使用，iOS 版处于测试阶段。尚未达到 1.0——参见[路线图](#roadmap)。

---

## 安装

可前往 **[Releases](https://github.com/martinkoutecky/tine/releases)** 页面下载预编译安装包。macOS 构建已签名并经过公证；Windows 构建尚未进行代码签名，因此首次运行时 Windows 可能会弹出安全警告。可按以下方式继续安装或启动：

- **Linux** —— **AppImage** 无需安装，可在任何发行版上运行：先执行 `chmod +x Tine_*.AppImage`，然后启动它。也可使用 **`.deb`**（Debian/Ubuntu）或 **`.rpm`**（Fedora/openSUSE）。
- **macOS** —— 打开通用版 **`.dmg`**，将 Tine 拖入“应用程序”文件夹。
- **Windows** —— 运行 **`.exe`** 安装程序；如果出现 SmartScreen 提示，请点击**更多信息 → 仍要运行**。若不想安装，也可以下载便携版 **`Tine_*_x64-portable.zip`**，解压后直接运行 `Tine.exe`。Tine 需要 WebView2 运行时，而 Windows 10/11 已预装该运行时。
- **Android** —— 从 **[F-Droid](https://f-droid.org/packages/page.tine.app/)** 安装，或从 Releases 页面侧载 **`.apk`**。
- **iPhone / iPad** —— 通过 **[TestFlight](https://testflight.apple.com/join/rpGGpTVW)** 加入公开测试版（先安装 Apple 的 TestFlight 应用，再打开该链接）。目前尚未上架 App Store。

想从源码构建？参见 **[docs/DEVELOPING.md](docs/DEVELOPING.md)**。启动遇到问题？参见 **[docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md)**。

---

## 亮点

Tine 在 Logseq 基础上增加的功能——它们都源于这样一个想法：“*要是 Logseq 本就支持这些就好了。*”

- **⚡ 原生速度** —— 以纯 Rust 核心和小巧的原生运行时取代 Electron；输入始终在前端处理，读取整个图谱的操作直接命中内存索引。
- **🪟 分屏与标签页** —— 每个窗格都有独立的标签页和历史记录；`Alt+click` 在另一窗格打开，中键在后台标签页打开，可固定标签、在窗格间拖动，并支持浏览器式后退/前进。
- **🔎 可以变成查询的搜索** —— 将任意 Ctrl+K 结果在持久标签页中打开，进一步细化，以列表/表格/看板查看，命名后即成为查询页面。可视化查询编辑器与查询文本保持同步。
- **▦ Sheets** —— 在普通项目符号块之上提供网格、类型化字段表、看板、公式列、筛选、聚合以及 CSV 导入。
- **🤝 Concord** —— 同步冲突副本（Syncthing、Dropbox）和 git 冲突标记可在原处逐块审阅，每处差异都附带建议；冲突绝不会被静默覆盖，启动快照和“删除到回收站”为一切提供备份。
- **🌐 实时导出** —— 将页面发布为静态网站，仍可像 Tine 一样搜索、打开标签页和分屏（[使用指南](https://tine.page/guide/)就是这样发布的）；命令行中使用 `tine export live`。
- **⚡ 全局快速笔记**、**🎯 专注模式 + 弱化非编辑块**、**🔁 将未完成任务顺延到今天**、**📖 应用内指南**，以及首次运行时预装、同样可在 Logseq 中打开的 **👋 演示图谱**。

<p align="center">
  <img src="docs/img/quick-capture.png" alt="全局快速笔记迷你窗口" width="32%">
  <img src="docs/img/focus-dim.png" alt="弱化非活动块的专注模式" width="32%">
  <img src="docs/img/tabs.png" alt="内置标签页" width="32%">
</p>

<p align="center">
  <img src="docs/img/sheets.png" alt="表格网格、字段表与任务看板" width="640">
</p>

<p align="center">
  <img src="docs/img/waveform.png" alt="支持跳转与速度控制的音频波形叠加播放器" width="640"><br>
  <sub>粘贴音频后，点击 <b>⤢ Expand（展开）</b> 即可使用音波进度条（±5 秒 / ±15 秒跳转、播放速度、时间显示）——Logseq 没有对应的核心功能。</sub>
</p>

---

## 功能

下面是现有功能的快速概览——完整功能列表见 **[docs/FEATURES.md](docs/FEATURES.md)**；也可在浏览器中通过**[使用指南](https://tine.page/guide/)**查看内容的实际渲染效果。

| 领域 | 主要功能 |
|------|-----------|
| **大纲** | 点击即可编辑，并将光标准确定位到点击位置；与 Logseq 一致的键盘操作语义；缩放、拖动排序和多块选择；块内列表与检查清单；提示块；实时 `/calc` 块。 |
| **媒体** | 粘贴/导入图像、视频与音频；可配置资源文件名；拖动调整图像*和*视频尺寸；音频波形悬浮播放器；图像灯箱；清理孤立媒体。 |
| **链接、引用与查询** | 支持自动补全的 `[[page]]` · `#tag` · `((block ref))` · `{{embed}}`；实时显示已链接与未链接的引用；每个块的引用计数；宏集合；同一个查询引擎支撑友好筛选器、可视化构建器、原始 DSL 以及搜索/列表/表格/看板视图；限定范围的 Datalog 查询路径。 |
| **任务、日记与日期** | 任务工作流与优先级、通过日期选择器设置计划日期/截止日期、循环任务、任务顺延、多日日记流、议程和日历。 |
| **PDF** | 可缩放的虚拟化查看器、PDF 内搜索、以兼容 Logseq 的方式保存文本高亮和区域（图像）高亮；每处高亮都会生成一个可继续添加注释的块。 |
| **搜索与导航** | `Ctrl+K` 切换器（标题与全文搜索），结果可在持久标签页中打开并提升为查询页面；命令面板、应用内指南、命名空间树、标签页、分屏、后退/前进、专注模式、全局快速笔记、页面图标。 |
| **你的文件** | 可安全地与通过 Syncthing 同步的 Logseq 移动端配合使用——Concord 冲突审阅、保留格式的原子保存、事务式重命名、Org mode（逐字节保真或只读）、快照与回收站。 |
| **自定义与导出** | 可重新映射快捷键，支持 `?` 帮助；内置主题库与自定义 CSS；多语言拼写检查；静态与实时 HTML 导出；复制/导出为 Markdown；**将页面导出为 PDF**（桌面端）。 |

→ **[在 docs/FEATURES.md 中查看所有功能及详细说明。](docs/FEATURES.md)**
→ **[Tine 与 Logseq 的逐项功能对比](https://tine.page/compare.html)** —— 一份坦诚且标注日期的比较：哪些功能已经对应、哪些仅在有限范围内支持、哪些尚未实现，以及 Tine 有意不做什么。

---

## 社区

所有交流都在 GitHub 上进行。问题、想法、截图和作品分享请发到 **[GitHub Discussions](https://github.com/martinkoutecky/tine/discussions)**；具体错误和功能请求请[提交 issue](https://github.com/martinkoutecky/tine/issues)。

## 贡献

Tine 是由一位维护者独立维护的项目，采用一种不同寻常的贡献模式：**最有价值的贡献是测试和错误报告**；代码改动应以**提案/规格说明**的形式提出，再由维护者自行实现，而不是直接合并外部补丁（文档和拼写修正 PR 除外）。这样做的原因以及如何提交高质量报告，请参阅 **[CONTRIBUTING.md](CONTRIBUTING.md)**。从源码构建：**[docs/DEVELOPING.md](docs/DEVELOPING.md)**。

<a id="roadmap"></a>
## 路线图

Tine 目前处于 0.6：Sheets、分屏、查询引擎、Concord、实时导出、移动端以及实验性的[权限受限插件 API](https://tine.page/plugins.html) 均已发布。接下来：0.7 将完善查询引擎和实时导出，随后 0.8 带来本地化；图谱视图正在评估中。**明确不做（设计如此）：**白板、记忆卡片、兼容 Logseq/Obsidian 插件，以及内置同步。完整的开发待办——接下来要做什么、延后什么，以及 WONTFIX 的内容——位于 [`docs/BACKLOG.md`](docs/BACKLOG.md)。

<a id="support"></a>
## 支持

Tine 免费且开源，是我利用忙碌生活中的零碎时间开发的项目。如果它让你记笔记更快、日常使用更顺手，而你愿意请我喝杯咖啡表达谢意——我会非常开心，也由衷感激。所有捐赠都会优先用于 Tine 的运营成本，例如域名，以及未来可能需要的应用商店注册费用。

有件事需要坦诚说明，以免彼此误解：**捐赠不会改变我对 Tine 工作的优先级，也不能换取某项功能。** 我会在有时间时开发 Tine，并选择自己认为值得做的事情——赞助不会改变这一点。请把它视为对现有成果的感谢，而不是对未来工作的预付款。无论是否赞助，彼此都没有任何期待或义务。🌱

[![Ko-fi](https://img.shields.io/badge/Ko--fi-support-FF5E5B?logo=kofi&logoColor=white)](https://ko-fi.com/martinkoutecky)
· [GitHub Sponsors](https://github.com/sponsors/martinkoutecky)

## 致谢

Tine 是一个独立重新实现的项目，并非 fork——整个代码库使用 Rust + SolidJS 原创编写，不包含任何 Logseq 源代码。不过，它以 Logseq 的磁盘格式为兼容目标，并改编了 Logseq 大纲 CSS 的一部分（变量以及项目符号/缩进规则），因此从许可角度看属于衍生作品，并采用相同的许可证发布。

[Logseq](https://github.com/logseq/logseq) 版权归其作者所有，采用 AGPL-3.0 许可证。Tine **与 Logseq 没有关联，也未获得 Logseq 官方认可或背书。** 感谢 Logseq 项目提供了这种格式，以及它所开创的设计。

## 许可证

[GNU AGPL-3.0-only](LICENSE)。

版权所有 (C) 2026 Martin Koutecký。

本程序是自由软件：你可以根据自由软件基金会发布的 GNU Affero General Public License 第 3 版条款重新发布和/或修改本程序。本程序的发布是希望它能够发挥作用，但**不提供任何担保**；甚至不包含对**适销性**或**特定用途适用性**的默示担保。详情请参阅 [LICENSE](LICENSE)。

---

<sub>从零构建，作为速度更快、兼容 Logseq 文件格式的替代方案。与 Logseq 没有关联。</sub>
