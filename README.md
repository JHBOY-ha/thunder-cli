# thunder
[![CI](https://github.com/gngpp/thunder/actions/workflows/CI.yml/badge.svg)](https://github.com/gngpp/thunder/actions/workflows/CI.yml)
<a href="/LICENSE">
    <img src="https://img.shields.io/github/license/gngpp/thunder?style=flat">
  </a>
  <a href="https://github.com/gngpp/thunder/releases">
    <img src="https://img.shields.io/github/release/gngpp/thunder.svg?style=flat">
  </a><a href="hhttps://github.com/gngpp/thunder/releases">
    <img src="https://img.shields.io/github/downloads/gngpp/xunlei/total?style=flat&?">
  </a>
  [![Docker Image](https://img.shields.io/docker/pulls/gngpp/xunlei.svg)](https://hub.docker.com/r/gngpp/xunlei/)

thunder从迅雷群晖套件中提取，用于发行版Linux（支持OpenWrt/Alpine/Docker）的迅雷远程下载服务。仅供测试，测试完请自觉删除。

- 支持X86_64/aarch64
- 支持glibc/musl
- 支持更改下载目录
- 支持面板认证
- 支持以特定用户安装(UID/GID)
- Docker镜像最小压缩（40MB左右）
- 支持插件：NAS小星（pcdn），测速插件
- 内侧邀请码（3H9F7Y6D/迅雷牛通），内侧码申请快速通道：https://t.cn/A6fhraWZ

> 默认Web访问端口5055

```shell
❯ ./thunder                   
Synology NAS thunder run on Linux

Usage: thunder
       thunder <COMMAND>

Commands:
  install    Install thunder
  uninstall  Uninstall thunder
  run        Run thunder
  start      Start thunder daemon
  stop       Stop thunder daemon
  log        Show the Http server daemon log
  ps         Show the Http server daemon process
  dl         Command-line download client (add/list/pause/rm tasks)
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

### Arch Linux

Arch Linux 及其衍生版可以通过 [AUR](https://aur.archlinux.org/packages/thunder-nas-bin) 或[自建源](https://github.com/taotieren/aur-repo)安装 `thunder-nas`

```bash
yay -Syu thunder-nas
```

### Ubuntu(Other Linux)

GitHub [Releases](https://github.com/gngpp/thunder/releases) 中有预编译的 deb包，二进制文件，以Ubuntu为例：

```shell
wget https://github.com/gngpp/thunder/releases/download/v1.0.3/thunder_1.0.3_amd64.deb

dpkg -i thunder_1.0.3_amd64.deb

# 安装迅雷，默认在线下载安装，如果需要设置更多参数请带上`-h`，查看说明
thunder install

# 安装迅雷以指定spk包安装，如果需要设置更多参数请带上`-h`，查看说明
thunder install /root/nasxunlei-DSM7-x86_64.spk

# 卸载迅雷
thunder uninstall

# 前台运行迅雷，如果需要设置更多参数请带上`-h`，查看说明
thunder run 

# 后台运行迅雷，如果需要设置更多参数请带上`-h`，查看说明
thunder start

# 停止运行迅雷
thunder stop

# 查看运行状态
thunder ps

# 查看运行日志
thunder log
```

### 自行编译

```shell
git clone https://github.com/gngpp/thunder && cd thunder

cargo build --release && mv target/release/thunder .
```

### 命令行下载器 (`thunder dl`)

内置一个命令行下载客户端，直接对运行中的服务（默认 `http://127.0.0.1:5055`）操作任务，无需打开网页面板。

> 前提：已 `thunder start` 启动服务，并在网页面板（`http://<IP>:5055`）扫码登录过迅雷账号一次。登录态由后端持久化，之后 `dl` 命令即可直接使用。

```shell
# 添加磁力链接 / http(s) 直链下载
thunder dl add "magnet:?xt=urn:btih:..."

# 解析链接、查看内含文件列表（不下载）
thunder dl resolve "magnet:?xt=urn:btih:..."

# 种子含多个文件时，选择要下载的文件（序号见 resolve/add 输出）
thunder dl add "magnet:?xt=..." --pick 0,3,5     # 只下指定序号
thunder dl add "magnet:?xt=..." --all            # 全部下载
thunder dl add "magnet:?xt=..." -n "自定义文件夹名"

# 查看任务列表与进度
thunder dl list
thunder dl list --active        # 只看进行中的任务
thunder dl list --json          # JSON 输出，便于脚本处理

# 暂停 / 恢复 / 删除任务（任务 ID 见 list 输出）
thunder dl pause  <ID>...
thunder dl resume <ID>...
thunder dl rm     <ID>...
thunder dl rm     <ID> --delete-files   # 同时删除已下载文件

# 逃生舱：向任意 API 路径发送原始请求（调试/尝鲜用）
thunder dl raw GET  drive/v1/tasks
thunder dl raw POST drive/v1/task -b '{"...":"..."}'
```

全局选项：`--host`（服务地址，默认 `127.0.0.1:5055`）、`--password`（若面板设置了访问密码）、`--json`。

### FQA
 - 当前大重构，`OpenWrt` / `Docker` 后续再完善支持
 - musl运行库的操作系统，若已存在glibc运行库，那么会优先兼容选择使用操作系统运行库环境（避免对系统其他软件依赖冲突，可能会缺依赖，自行补全）
 - 指定运行LD加载库或压缩目前无法做到（二进制带签名），需要逆向打patch
 - 插件依赖bash，系统需要安装bash
 - **下载目录必须位于真实挂载的文件系统上**：迅雷引擎会校验下载目录所在的存储卷，若目录位于容器的 overlay 根文件系统（如默认的 `/opt/thunder/downloads`），会因 `IsPathValid` 校验失败而任务无法开始。请通过 `thunder install -d <目录>` 将下载目录设置到一个独立挂载点（例如挂载的数据盘 `/data/...`）之下。
 - **无 `CAP_SYS_ADMIN` 的容器**（如受限的 K8s Pod）无法执行 `mount --bind`，此时 thunder 会自动降级：直接使用下载目录本身，不再做绑定挂载，服务仍可正常启动。

