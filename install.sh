#!/bin/sh
# jmds 安装脚本（Linux / macOS）
#
# 安装布局（版本化 + 原子切换）：
#
#   <dir>/releases/<版本>-<目标平台>/jmds   每个版本各占一个目录，互不覆盖
#   <dir>/current                              符号链接，指向当前使用的版本
#   <dir>/install.lock                         纯文本，记录当前版本、安装时间与版本历史
#
# 用法：
#
#   curl -fsSL https://raw.githubusercontent.com/GBLMX/jmds/main/install.sh | sh
#   sh install.sh --dir ~/bin --version v1.4.0
#   sh install.sh --dir ~/bin --rollback
#
# 选项也可以写成环境变量（命令行优先）：
#
#   --version <tag|latest>   JMDS_VERSION        默认 latest
#   --dir <path>             JMDS_INSTALL_DIR    默认 ~/.local/bin
#   --checksums <url>        JMDS_CHECKSUMS      默认取资产同级的 SHA256SUMS
#   --repo <owner/name>      JMDS_REPO           默认 GBLMX/jmds
#   --host <url>             JMDS_GITHUB         默认 https://github.com
#   --mirror <url>           --host 的别名（镜像或代理）
#   --rollback               把 current 切回上一个版本
#   --dry-run                只打印计划，不下载不写入
#   --force                  同一版本也重新下载安装
#
# 行为：
#
#   * 先下载并校验，装进新的 releases/<版本>-<目标平台> 目录，最后一步才切换 current；
#     校验失败不会切换，切换失败会还原旧链接并以非 0 退出。
#   * 同一个版本重复安装时，目录已在且校验一致就跳过下载（--force 可强制重装）。
#   * 只保留最近 3 个版本目录，current 指向的那个永不删除。
#   * 提示与 PATH 都用 <dir>/current/jmds。

set -eu

REPO="${JMDS_REPO:-GBLMX/jmds}"
# 发布产物从哪里取。正常情况下是 https://github.com；镜像或代理走这个变量。
HOST="${JMDS_GITHUB:-https://github.com}"
VERSION="${JMDS_VERSION:-latest}"
INSTALL_DIR="${JMDS_INSTALL_DIR:-}"
CHECKSUMS="${JMDS_CHECKSUMS:-}"
DRY_RUN=0
FORCE=0
ROLLBACK=0

# 保留多少个版本目录（含当前版本）；current 指向的那个无论如何都留着。
KEEP=3
BIN=jmds

TMP=""
STAGING=""
OLD=""

cleanup() {
    if [ -n "$TMP" ]; then rm -rf "$TMP"; fi
    if [ -n "$STAGING" ]; then rm -rf "$STAGING"; fi
    if [ -n "$OLD" ]; then rm -rf "$OLD"; fi
    return 0
}
trap cleanup EXIT INT TERM

log() { printf '%s\n' "$*" >&2; }
die() { log "install.sh: $*"; exit 1; }

usage() {
    # 头部注释就是帮助文本：打印 shebang 之后连续的注释行。
    awk 'NR > 1 { if (sub(/^# ?/, "")) { print } else { exit } }' "$0"
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version 需要一个值}"; shift 2 ;;
        --dir) INSTALL_DIR="${2:?--dir 需要一个值}"; shift 2 ;;
        --checksums) CHECKSUMS="${2:?--checksums 需要一个值}"; shift 2 ;;
        --repo) REPO="${2:?--repo 需要一个值}"; shift 2 ;;
        --host | --mirror) HOST="${2:?$1 需要一个值}"; shift 2 ;;
        --rollback) ROLLBACK=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --force) FORCE=1; shift ;;
        -h | --help) usage ;;
        *) die "未知参数：$1（试试 --help）" ;;
    esac
done

# ------------------------------------------------------------------ 平台识别 ----

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$arch" in
        x86_64 | amd64) cpu="x86_64" ;;
        arm64 | aarch64) cpu="aarch64" ;;
        *) die "不支持的 CPU：$arch —— 本项目只发布 x86_64 与 aarch64 产物" ;;
    esac

    case "$os" in
        Linux)
            # musl 是另一套目标（另一套 C 库）。glibc 产物在 musl 上虽然能跑，但需要兼容层，
            # 与其塞一个可能起不来的二进制，不如直接说清楚。
            if ldd --version 2>&1 | grep -qi musl || [ -e /lib/ld-musl-x86_64.so.1 ] ||
                [ -e /lib/ld-musl-aarch64.so.1 ]; then
                die "检测到 musl libc —— 只发布 gnu 产物，请从源码安装：cargo install --git https://github.com/$REPO.git"
            fi
            target="$cpu-unknown-linux-gnu"
            ;;
        Darwin) target="$cpu-apple-darwin" ;;
        *) die "不支持的系统：$os —— Windows 请用 install.ps1" ;;
    esac

    printf '%s' "$target"
}

TARGET="$(detect_target)"

# Linux 的 aarch64 产物由 cross 构建，其余是本地构建；两个平台的压缩包都是 tar.gz，
# 里面只有一个叫 jmds 的文件。
case "$TARGET" in
    *-linux-gnu | *-apple-darwin) ASSET="jmds-$TARGET.tar.gz" ;;
    *) die "没有 $TARGET 对应的发布资产" ;;
esac

# ------------------------------------------------------------------ 目录布局 ----

abs_dir() {
    # 把 --dir 补成绝对路径；不创建任何目录，所以 --dry-run 也能安全调用。
    d="$1"
    case "$d" in
        /) printf '%s' "$d"; return 0 ;;
        /*) ;;
        *) d="$PWD/$d" ;;
    esac
    printf '%s' "$d" | sed -e 's#/\./#/#g' -e 's#//*#/#g' -e 's#/$##'
}

if [ -z "$INSTALL_DIR" ]; then
    INSTALL_DIR="${HOME:?没有 HOME，请用 --dir 指定安装目录}/.local/bin"
fi
INSTALL_DIR="$(abs_dir "$INSTALL_DIR")"

RELEASES="$INSTALL_DIR/releases"
CURRENT_LINK="$INSTALL_DIR/current"
LOCK_FILE="$INSTALL_DIR/install.lock"

if [ "$VERSION" = latest ]; then
    BASE="$HOST/$REPO/releases/latest/download"
else
    BASE="$HOST/$REPO/releases/download/$VERSION"
fi
ASSET_URL="$BASE/$ASSET"
[ -n "$CHECKSUMS" ] || CHECKSUMS="$BASE/SHA256SUMS"

# 显式版本号现在就能算出来；latest 要等下载下来问二进制自己。
VERSION_NUM=""
if [ "$VERSION" != latest ]; then
    VERSION_NUM="${VERSION#v}"
    case "$VERSION_NUM" in
        [0-9]*) ;;
        *) die "版本号看着不对：$VERSION（期望形如 v1.4.0 或 1.4.0）" ;;
    esac
fi

# -------------------------------------------------------------- 布局相关工具 ----

mtime_of() {
    # 目录的修改时间（epoch 秒），GNU 与 BSD 的 stat 参数不一样。
    if stat -c %Y "$1" >/dev/null 2>&1; then
        stat -c %Y "$1"
    elif stat -f %m "$1" >/dev/null 2>&1; then
        stat -f %m "$1"
    else
        printf '0'
    fi
}

current_target() {
    # current 指向的目录名（不含 releases/ 前缀）；没有链接就什么都不输出。
    if [ -L "$CURRENT_LINK" ]; then
        t="$(readlink "$CURRENT_LINK" 2>/dev/null || true)"
        printf '%s' "${t##*/}"
    fi
    return 0
}

lock_history() {
    # install.lock 里的版本历史，旧 -> 新，一行一个。
    if [ -f "$LOCK_FILE" ]; then
        sed -n 's/^history=//p' "$LOCK_FILE" | head -n 1 | tr ' ' '\n' | sed '/^$/d'
    fi
    return 0
}

in_history() {
    case " $(lock_history | tr '\n' ' ') " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

ordered_dirs() {
    # releases 下的版本目录，新 -> 旧。先按 install.lock 记录的顺序（权威），
    # 不在记录里的（比如 lock 被删过）再按目录修改时间排。
    hist="$(lock_history)"
    new_first=""
    for h in $hist; do
        new_first="$h $new_first"
    done
    for h in $new_first; do
        if [ -d "$RELEASES/$h" ] && [ ! -L "$RELEASES/$h" ]; then
            printf '%s\n' "$h"
        fi
    done
    for p in "$RELEASES"/*; do
        if [ -d "$p" ] && [ ! -L "$p" ]; then
            name="${p##*/}"
            case "$name" in
                .*) continue ;;
            esac
            if ! in_history "$name"; then
                printf '%s %s\n' "$(mtime_of "$p")" "$name"
            fi
        fi
    done | sort -rn | cut -d' ' -f2-
}

switch_current() {
    # 把 current 换到 releases/<名字>：新链接先建好，再原子地改名过去。
    name="$1"
    new="$INSTALL_DIR/.current.$$"
    rm -f "$new" 2>/dev/null || true
    ln -s "releases/$name" "$new" || return 1
    if mv -T "$new" "$CURRENT_LINK" 2>/dev/null; then
        return 0
    fi
    # BSD/macOS 的 mv 没有 -T：退化成「先删后建」，中间只有一次 rename 的间隔。
    rm -f "$CURRENT_LINK" 2>/dev/null || true
    mv "$new" "$CURRENT_LINK" || return 1
    return 0
}

restore_current() {
    # 出错时把 current 恢复成原来指向的版本。
    rm -f "$CURRENT_LINK" 2>/dev/null || true
    ln -s "releases/$1" "$CURRENT_LINK"
}

new_history() {
    # 版本历史（旧 -> 新）= 现有目录（去掉重复和已删的）+ 本次装的版本。
    rel="$1"
    out=""
    for h in $(ordered_dirs); do
        if [ "$h" = "$rel" ]; then continue; fi
        out="$h $out"
    done
    out="${out% }"
    if [ -n "$out" ]; then
        printf '%s %s\n' "$out" "$rel"
    else
        printf '%s\n' "$rel"
    fi
}

write_lock() {
    # write_lock <版本目录名> <版本号> <上一个版本目录名，可为空>
    rel="$1"
    num="$2"
    prev="$3"
    tmp="$LOCK_FILE.tmp.$$"
    {
        printf 'version=%s\n' "$num"
        printf 'target=%s\n' "$TARGET"
        printf 'dir=releases/%s\n' "$rel"
        printf 'installed_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'previous=%s\n' "$prev"
        printf 'history=%s\n' "$(new_history "$rel")"
    } >"$tmp" || { rm -f "$tmp"; die "写不了 $LOCK_FILE（权限不足？）"; }
    mv -f "$tmp" "$LOCK_FILE" || die "更新不了 $LOCK_FILE"
}

prune_versions() {
    # 只保留最近 KEEP 个版本目录；current 指向的、以及刚装好的那个永不删。
    keep_rel="$1"
    cur="$(current_target)"
    n=0
    for name in $(ordered_dirs); do
        n=$((n + 1))
        if [ "$n" -le "$KEEP" ]; then continue; fi
        if [ "$name" = "$keep_rel" ]; then continue; fi
        if [ "$name" = "$cur" ]; then continue; fi
        if rm -rf "$RELEASES/$name"; then
            log "  清理       旧版本 $RELEASES/$name"
        else
            log "  警告       旧版本 $RELEASES/$name 删不掉，先留着"
        fi
    done
    return 0
}

version_of() {
    # 运行二进制取版本号：`jmds 1.4.0` -> `1.4.0`
    out="$("$1" --version 2>/dev/null)" || return 1
    printf '%s\n' "$out" | sed -n 's/^[^0-9]*\([0-9][0-9A-Za-z.+-]*\).*$/\1/p' | head -n 1
}

finish_install() {
    # finish_install <版本目录名> <版本号>：切换 current、写锁、清理、打印结论。
    rel="$1"
    num="$2"
    prev="$(current_target)"

    if ! switch_current "$rel"; then
        if [ -n "$prev" ]; then
            if restore_current "$prev"; then
                log "  回退       current 已还原成 releases/$prev"
            else
                log "  警告       旧链接也没能还原，请手动把 $CURRENT_LINK 指向 releases/$prev"
            fi
        fi
        die "切换 current 到 releases/$rel 失败 —— 安装中止，current 未被改动"
    fi

    write_lock "$rel" "$num" "$prev"
    prune_versions "$rel"

    log "  已安装     $RELEASES/$rel/$BIN"
    log "  版本       $num"
    log "  current    $CURRENT_LINK -> releases/$rel"
    log "  可执行     $INSTALL_DIR/current/$BIN"
    if [ "$prev" = "$rel" ]; then
        log "  上一个版本 （没有变化，current 本来就指向它）"
    else
        log "  上一个版本 ${prev:-（无）}"
    fi
    log "  回滚命令   sh install.sh --dir $INSTALL_DIR --rollback"

    case ":$PATH:" in
        *":$INSTALL_DIR/current:"*) ;;
        *) log "  提示       $INSTALL_DIR/current 不在 PATH 里，加一下：
               export PATH=\"$INSTALL_DIR/current:\$PATH\"" ;;
    esac
    return 0
}

# ------------------------------------------------------------------ 回滚 ----

if [ "$ROLLBACK" = 1 ]; then
    if [ ! -d "$RELEASES" ]; then
        die "没有可回滚的东西：$RELEASES 不存在（还没有用本脚本安装过）"
    fi
    cur="$(current_target)"
    target=""
    for name in $(ordered_dirs); do
        if [ "$name" != "$cur" ]; then
            target="$name"
            break
        fi
    done
    if [ -z "$target" ]; then
        die "没有可回滚的版本：$RELEASES 里只有 ${cur:-（空目录）}"
    fi

    if [ "$DRY_RUN" = 1 ]; then
        log "install.sh: 回滚（dry-run）"
        log "  current    ${cur:-（无）} -> releases/$target"
        log "  (dry-run：没有改动任何东西)"
        exit 0
    fi

    if ! switch_current "$target"; then
        if [ -n "$cur" ] && restore_current "$cur"; then
            log "  回退       current 已还原成 releases/$cur"
        fi
        die "回滚失败：未能把 $CURRENT_LINK 指向 releases/$target"
    fi
    ver="${target%-$TARGET}"
    write_lock "$target" "$ver" "$cur"
    log "install.sh: 已回滚"
    log "  版本       $ver"
    log "  current    $CURRENT_LINK -> releases/$target"
    log "  可执行     $INSTALL_DIR/current/$BIN"
    log "  上一个版本 ${cur:-（无）}"
    log "  回滚命令   sh install.sh --dir $INSTALL_DIR --rollback"
    exit 0
fi

# ------------------------------------------------------------------ 安装 ----

log "install.sh: $REPO $VERSION"
log "  platform   $(uname -s) $(uname -m) -> $TARGET"
log "  asset      $ASSET_URL"
log "  install    $RELEASES/${VERSION_NUM:-<下载后确定>}-$TARGET/$BIN"
log "  current    $CURRENT_LINK"
log "  verify     $CHECKSUMS"

if [ -e "$INSTALL_DIR" ] && [ ! -d "$INSTALL_DIR" ]; then
    die "$INSTALL_DIR 已经存在，而且不是目录"
fi
if [ -e "$CURRENT_LINK" ] && [ ! -L "$CURRENT_LINK" ]; then
    die "$CURRENT_LINK 已经存在，而且不是符号链接 —— 请先删掉它，或换一个 --dir"
fi

if [ "$DRY_RUN" = 1 ]; then
    log "  (dry-run：没有下载，也没有写入)"
    exit 0
fi

if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
    die "创建不了安装目录 $INSTALL_DIR（权限不足？换个 --dir 或带上 sudo）"
fi
if ! mkdir -p "$RELEASES" 2>/dev/null; then
    die "创建不了版本目录 $RELEASES（权限不足？）"
fi

# ------------------------------------------------------------------ 下载 ----

HTTP_CODE=""

fetch() {
    # fetch <url> <目标文件>：成功返回 0；失败返回非 0，curl 能拿到状态码时放进 HTTP_CODE。
    HTTP_CODE=""
    if command -v curl >/dev/null 2>&1; then
        code="$(curl -sSL -o "$2" -w '%{http_code}' --max-time 300 "$1" 2>/dev/null)" || return 1
        HTTP_CODE="$code"
        case "$code" in
            2*) return 0 ;;
            *) return 1 ;;
        esac
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1" 2>/dev/null && return 0
        return 1
    else
        die "需要 curl 或 wget 才能下载，先装一个再试"
    fi
}

fetch_optional() {
    # 拿不到（网络问题、老 release 没有这个文件）就返回非 0，让调用方决定怎么办。
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" --max-time 60 "$1" 2>/dev/null
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1" 2>/dev/null
    else
        return 1
    fi
}

TMP="$(mktemp -d)" || die "创建不了临时目录"
trap cleanup EXIT INT TERM

WANT=""
if fetch_optional "$CHECKSUMS" "$TMP/SHA256SUMS"; then
    WANT="$(awk -v name="$ASSET" '$2 == name { print $1 }' "$TMP/SHA256SUMS")"
    if [ -z "$WANT" ]; then
        die "SHA256SUMS 里没有 $ASSET 的记录（$CHECKSUMS）"
    fi
fi

# 已经装过同一个版本：目录里的记录和 release 的 SHA256SUMS 对得上就不用再下载。
MATCH=""
if [ -n "$WANT" ] && [ "$FORCE" != 1 ]; then
    if [ -n "$VERSION_NUM" ]; then
        cand="$VERSION_NUM-$TARGET"
        if [ -f "$RELEASES/$cand/$BIN" ] && [ -f "$RELEASES/$cand/.archive.sha256" ] &&
            [ "$(cat "$RELEASES/$cand/.archive.sha256")" = "$WANT" ]; then
            MATCH="$cand"
        fi
    else
        # --version latest：按记录反查这堆目录里有没有就是最新版的那个。
        for p in "$RELEASES"/*; do
            if [ -d "$p" ] && [ ! -L "$p" ] && [ -f "$p/$BIN" ] && [ -f "$p/.archive.sha256" ]; then
                name="${p##*/}"
                case "$name" in
                    .*) continue ;;
                esac
                if [ "$(cat "$p/.archive.sha256")" = "$WANT" ]; then
                    MATCH="$name"
                    break
                fi
            fi
        done
    fi
fi
# 没有校验和可比对（老 release 没发 SHA256SUMS）时，退回问二进制自己。
if [ -z "$MATCH" ] && [ "$FORCE" != 1 ] && [ -n "$VERSION_NUM" ] &&
    [ -f "$RELEASES/$VERSION_NUM-$TARGET/$BIN" ]; then
    if [ "$(version_of "$RELEASES/$VERSION_NUM-$TARGET/$BIN" || true)" = "$VERSION_NUM" ]; then
        MATCH="$VERSION_NUM-$TARGET"
    fi
fi

if [ -n "$MATCH" ]; then
    log "  已存在     $RELEASES/$MATCH 校验一致，跳过下载（--force 可强制重装）"
    finish_install "$MATCH" "${MATCH%-$TARGET}"
    exit 0
fi

log "  下载中…"
if ! fetch "$ASSET_URL" "$TMP/$ASSET"; then
    case "$HTTP_CODE" in
        404)
            die "release $VERSION 里没有 $ASSET（$ASSET_URL）—— 这个平台可能没有发布产物，或 tag 写错了"
            ;;
        "")
            die "下载失败：$ASSET_URL —— 网络不通？可以用 --mirror 指向镜像"
            ;;
        *)
            die "下载失败：$ASSET_URL（HTTP $HTTP_CODE）"
            ;;
    esac
fi

# ------------------------------------------------------------------ 校验 ----

if [ -n "$WANT" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
        got="$(sha256sum "$TMP/$ASSET" | cut -d' ' -f1)"
    elif command -v shasum >/dev/null 2>&1; then
        got="$(shasum -a 256 "$TMP/$ASSET" | cut -d' ' -f1)"
    else
        die "系统里既没有 sha256sum 也没有 shasum，没法校验 —— 先装 coreutils"
    fi
    if [ "$WANT" != "$got" ]; then
        die "校验失败：$ASSET 的 SHA256 对不上（期望 $WANT，实际 $got）—— 已中止，current 没有被改动"
    fi
    log "  校验       ok（$got）"
else
    # 早于 SHA256SUMS 的 release 仍然能装，但要明说这次没校验过，别让用户以为校验过了。
    log "  校验       拿不到 $CHECKSUMS —— 这次没有校验就安装"
fi

# ------------------------------------------------------------------ 解压 ----

# 先解到 releases 下的临时目录，装完再改名成正式目录：同一个文件系统里改名是原子的，
# 也就不会出现「目录在、二进制只有一半」的中间态。
STAGING="$RELEASES/.staging.$$"
rm -rf "$STAGING"
if ! mkdir -p "$STAGING" 2>/dev/null; then
    die "在 $RELEASES 下创建临时目录失败（权限不足？）"
fi
if ! tar -xzf "$TMP/$ASSET" -C "$STAGING" 2>/dev/null; then
    die "解压失败：$ASSET 不是有效的 tar.gz"
fi
if [ ! -f "$STAGING/$BIN" ]; then
    die "压缩包里没有 $BIN"
fi
chmod 755 "$STAGING/$BIN"

if [ -z "$VERSION_NUM" ]; then
    # --version latest：这时候才知道装的是哪个版本。
    VERSION_NUM="$(version_of "$STAGING/$BIN" || true)"
    case "$VERSION_NUM" in
        [0-9]*) ;;
        *)
            die "$BIN --version 没有给出可用版本号 —— 请显式指定 --version <tag>"
            ;;
    esac
    log "  版本       latest = $VERSION_NUM"
fi

REL_NAME="$VERSION_NUM-$TARGET"
VERSION_DIR="$RELEASES/$REL_NAME"

if [ -n "$WANT" ]; then
    # 记下压缩包的哈希，下次同一个版本就能直接跳过下载。
    printf '%s\n' "$WANT" >"$STAGING/.archive.sha256"
fi

OLD="$RELEASES/.old.$$"
if [ -d "$VERSION_DIR" ]; then
    rm -rf "$OLD"
    if ! mv "$VERSION_DIR" "$OLD"; then
        die "挪不动旧目录 $VERSION_DIR"
    fi
fi
if ! mv "$STAGING" "$VERSION_DIR"; then
    if [ -d "$OLD" ]; then
        mv "$OLD" "$VERSION_DIR" 2>/dev/null || true
    fi
    die "装不进 $VERSION_DIR"
fi
rm -rf "$OLD"

finish_install "$REL_NAME" "$VERSION_NUM"
