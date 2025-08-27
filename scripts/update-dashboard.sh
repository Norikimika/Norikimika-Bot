#!/system/bin/sh
set -e

# Function to update Zashboard
update_zashboard() {
    echo "🔄 Updating Zashboard..."
    TMP_DIR=$(mktemp -d)
    cd "$TMP_DIR" || exit 1

    # Download lastest
    /data/user/0/com.termux/files/usr/bin/wget -q --show-progress https://github.com/Zephyruso/zashboard/archive/refs/heads/gh-pages.zip || {
        echo "❌ Failed to download gh-pages"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Unzip
    unzip -oq gh-pages.zip || {
        echo "❌ Failed to unzip gh-pages.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Replace existing dashboard
    rm -rf /data/adb/php7/files/www/zashboard/*
    cp -r zashboard-gh-pages/* /data/adb/php7/files/www/zashboard/ || {
        echo "❌ Failed to copy new dashboard files"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Cleanup
    rm -rf "$TMP_DIR"
    echo "✅ Zashboard updated successfully"
}

# Function to update MetaCubeXD
update_metacubexd() {
    echo "🔄 Updating MetaCubeXD..."
    TMP_DIR=$(mktemp -d)
    cd "$TMP_DIR" || exit 1

    # Download latest
    /data/user/0/com.termux/files/usr/bin/wget -q --show-progress https://github.com/MetaCubeX/metacubexd/archive/refs/heads/gh-pages.zip || {
        echo "❌ Failed to download gh-pages.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Unzip
    unzip -oq gh-pages.zip || {
        echo "❌ Failed to unzip gh-pages.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Replace existing dashboard
    rm -rf /data/adb/box/clash/dashboard/*
    cp -r metacubexd-gh-pages/* /data/adb/box/clash/dashboard/ || {
        echo "❌ Failed to copy new dashboard files"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Cleanup
    rm -rf "$TMP_DIR"
    echo "✅ MetaCubeXD updated successfully"
}

# Function to update Yacd
update_yacd() {
    echo "🔄 Updating Yacd..."
    TMP_DIR=$(mktemp -d)
    cd "$TMP_DIR" || exit 1

    # Download lastest
    /data/user/0/com.termux/files/usr/bin/wget -q --show-progress https://github.com/CHIZI-0618/yacd/archive/refs/heads/gh-pages.zip || {
        echo "❌ Failed to download gh-pages.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Unzip
    unzip -oq gh-pages.zip || {
        echo "❌ Failed to unzip gh-pages.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Replace existing dashboard
    rm -rf /data/adb/box/clash/dashboard/*
    cp -r yacd-gh-pages/* /data/adb/box/clash/dashboard/ || {
        echo "❌ Failed to copy new dashboard files"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Cleanup
    rm -rf "$TMP_DIR"
    echo "✅ Yacd updated successfully"
}

# Run both updates
update_zashboard
update_metacubexd
# update_yacd

echo "🎉 All dashboards updated successfully"
exit 0