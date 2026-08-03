#!/bin/sh

set -eu

REPOSITORY=${NOYA_REPOSITORY:-jacoobwang/noya}
VERSION=${NOYA_VERSION:-latest}
INSTALL_DIR=${NOYA_INSTALL_DIR:-"${HOME}/.local/bin"}

say() {
    printf '%s\n' "noya-installer: $*"
}

warn() {
    printf '%s\n' "noya-installer: warning: $*" >&2
}

die() {
    printf '%s\n' "noya-installer: error: $*" >&2
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

detect_target() {
    os=$(uname -s 2>/dev/null || true)
    architecture=$(uname -m 2>/dev/null || true)

    case "$architecture" in
        arm64 | aarch64) architecture=aarch64 ;;
        x86_64 | amd64) architecture=x86_64 ;;
        *) die "unsupported CPU architecture: $architecture" ;;
    esac

    [ "$os" = Darwin ] || die "unsupported operating system: $os (Noya currently supports macOS only)"
    printf '%s-apple-darwin\n' "$architecture"
}

download() {
    url=$1
    destination=$2
    if has curl; then
        curl --fail --silent --show-error --location "$url" --output "$destination"
    elif has wget; then
        wget --quiet "$url" --output-document="$destination"
    else
        die "curl or wget is required to download Noya"
    fi
}

sha256() {
    path=$1
    if has shasum; then
        shasum -a 256 "$path" | awk '{print $1}'
    elif has sha256sum; then
        sha256sum "$path" | awk '{print $1}'
    else
        die "shasum or sha256sum is required to verify the download"
    fi
}

verify_archive() {
    archive=$1
    checksums=$2
    asset=$3
    expected=$(awk -v name="$asset" '$2 == name || $2 == ("*" name) {print $1; exit}' "$checksums")
    [ -n "$expected" ] || die "release checksum is missing for $asset"
    actual=$(sha256 "$archive")
    [ "$actual" = "$expected" ] || die "checksum verification failed for $asset"
}

find_homebrew() {
    if has brew; then
        command -v brew
    elif [ -x /opt/homebrew/bin/brew ]; then
        printf '%s\n' /opt/homebrew/bin/brew
    elif [ -x /usr/local/bin/brew ]; then
        printf '%s\n' /usr/local/bin/brew
    fi
}

ensure_ripgrep() {
    if [ "${NOYA_SKIP_RIPGREP:-0}" = 1 ]; then
        warn "skipping ripgrep dependency check because NOYA_SKIP_RIPGREP=1"
        return
    fi
    if has rg; then
        say "ripgrep is already installed"
        return
    fi

    brew_path=$(find_homebrew || true)
    if [ -n "$brew_path" ]; then
        say "ripgrep was not found; installing it with Homebrew"
        if "$brew_path" install ripgrep; then
            say "ripgrep installed"
        else
            warn "Homebrew could not install ripgrep; Noya is installed, but search_text will be unavailable"
        fi
    else
        warn "ripgrep is not installed and Homebrew was not found"
        warn "install Homebrew from https://brew.sh, then run: brew install ripgrep"
    fi
}

configure_path() {
    case ":${PATH:-}:" in
        *":${INSTALL_DIR}:"*)
            say "$INSTALL_DIR is already on PATH"
            return
            ;;
    esac

    shell_name=${SHELL:-}
    shell_name=${shell_name##*/}
    case "$shell_name" in
        zsh)
            shell_config=${ZDOTDIR:-$HOME}/.zshrc
            ;;
        bash)
            if [ -f "$HOME/.bash_profile" ]; then
                shell_config=$HOME/.bash_profile
            else
                shell_config=$HOME/.bashrc
            fi
            ;;
        *)
            warn "$INSTALL_DIR is not on PATH; add it to your shell configuration"
            return
            ;;
    esac

    if [ -f "$shell_config" ] && grep -F "$INSTALL_DIR" "$shell_config" >/dev/null 2>&1; then
        say "$INSTALL_DIR is already configured in $shell_config"
        return
    fi

    if ! printf '\n# Added by noya-installer\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$shell_config"; then
        warn "could not update $shell_config; add $INSTALL_DIR to your shell configuration"
        return
    fi

    say "added $INSTALL_DIR to $shell_config"
    warn "restart your shell or run: export PATH=\"$INSTALL_DIR:\$PATH\""
}

main() {
    has uname || die "uname is required to detect the platform"
    has tar || die "tar is required to unpack Noya"
    has install || die "install is required to place the Noya executable"

    target=$(detect_target)
    asset="noya-${target}.tar.gz"
    if [ -n "${NOYA_DOWNLOAD_BASE_URL:-}" ]; then
        download_base=${NOYA_DOWNLOAD_BASE_URL%/}
    elif [ "$VERSION" = latest ]; then
        download_base="https://github.com/${REPOSITORY}/releases/latest/download"
    else
        download_base="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
    fi

    temporary_directory=$(mktemp -d 2>/dev/null || mktemp -d -t noya-installer)
    trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM
    archive="$temporary_directory/$asset"
    checksums="$temporary_directory/SHA256SUMS"
    extracted="$temporary_directory/extracted"

    say "downloading $asset"
    download "$download_base/$asset" "$archive"
    download "$download_base/SHA256SUMS" "$checksums"
    verify_archive "$archive" "$checksums" "$asset"
    say "checksum verified"

    mkdir -p "$extracted" "$INSTALL_DIR"
    tar -xzf "$archive" -C "$extracted"
    [ -f "$extracted/noya" ] || die "$asset does not contain a noya executable"

    temporary_binary="$INSTALL_DIR/.noya-install.$$"
    install -m 0755 "$extracted/noya" "$temporary_binary"
    mv -f "$temporary_binary" "$INSTALL_DIR/noya"
    say "installed Noya to $INSTALL_DIR/noya"

    ensure_ripgrep

    configure_path
    say "run 'noya login <model>' to configure a model"
}

main "$@"
