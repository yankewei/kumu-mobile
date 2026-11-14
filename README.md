# Kumu Mobile

Kumu 的 iPhone 客户端，基于 [Native Shell (NSH)](https://nsh.dev) 和 Rust。
它通过 iroh 连接 Mac 上运行的 Kumu，attach 当前窗格，并用 NSH Terminal
直接显示和输入。

当前是 M1 垂直切片：配对、连接、抓取工作区状态、attach 最近窗格、窗格列表
和切换、终端输入/resize、断开。桌面设置页、Keychain、二维码和后台推送还没接。

## 运行

### 1. Mac 端启动远程服务

```sh
cd ../kumu
KUMU_REMOTE=1 cargo run --release
```

终端会打印：

```text
[remote] pairing code: ABCD-EFGH
[remote] link: kumu://pair/...
```

侧栏底部也会显示配对码；点击可复制配对链接。

### 2. iPhone / 模拟器

```sh
nsh package --target ios-simulator --run
```

首次打开粘贴 Mac 上的配对链接。链接里自带配对码，配对一次后 Token 存在
本机，后续直接连接。

模拟器调试可以直接注入链接，跳过粘贴：

```sh
SIMCTL_CHILD_KUMU_AUTOLINK='kumu://pair/...' \
  xcrun simctl launch --terminate-running-process booted com.yankewei.kumu.mobile
```

需要新的配对码时，重启 Mac 端或后续从桌面设置里重新生成。

## 协议

- iroh 1.4，ALPN `kumu/remote/1`，一条 QUIC 双向流。
- channel 0 是 JSON-RPC 控制；终端 channel 带 9 字节帧头：
  `channel | kind | length | payload`。
- 终端 kind：DATA / CREDIT / CLOSE / RESIZE / REPLAY。
- attach 在 Mac 的终端 session 线程上原子完成：resize → 快照 → ANSI 重放 →
  实时字节。
- 手机在第一次布局后把真实 cols/rows 用于 attach，避免按 80 列重放后换行。

## 当前 UI

- 顶部：`☰` 打开窗格抽屉，`⋯` 打开操作菜单；标题为“项目 › 窗格”；状态在第二行。
- 窗格抽屉：按当前连接状态列出窗格、项目、状态和未读；点击切换，底部断开连接。
- 操作菜单：在桌面显示、新建终端、断开连接。
- 终端：两行按键区，第一行控制键（含粘滞 Ctrl/Alt），第二行方向键和常用符号；键盘 inset 和底部安全区分别处理。
- 调试：`KUMU_AUTO_SHEET=1` 或 `KUMU_AUTO_MENU=1` 启动时直接打开对应面板。

## 当前限制

- 窗格抽屉还是平铺列表，没有按项目折叠。
- 新建终端后窗格列表不会立即刷新，需要重新打开抽屉/重连。
- 粘滞 Ctrl/Alt 只作用于单字符输入，多字符粘贴和 IME 不转换。
- Token 存在 NSH `Storage`，不是 Keychain。
- 没有心跳重连 UI；网络断开后回连接页重新连接。
- 终端查询应答重复、scrollback 不同步仍需实测。
- 后台/锁屏会断开，回前台重新 attach。

## 检查

```sh
cargo clippy --all-targets -- -D warnings
nsh package --target ios-simulator
```
