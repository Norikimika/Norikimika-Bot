#!/system/bin/sh
set -e

# Function to update Zashboard
update_zashboard() {
    echo "🔄 Updating Zashboard..."
    TMP_DIR=$(mktemp -d)
    cd "$TMP_DIR" || exit 1

    # Get latest tag
    tag_name=$(/data/user/0/com.termux/files/usr/bin/curl -s https://api.github.com/repos/Zephyruso/zashboard/releases/latest | /data/user/0/com.termux/files/usr/bin/jq -r .tag_name)

    # Download release
    /data/user/0/com.termux/files/usr/bin/wget -q --show-progress https://github.com/Zephyruso/zashboard/releases/download/$tag_name/dist.zip || {
        echo "❌ Failed to download dist.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Unzip
    unzip -oq dist.zip || {
        echo "❌ Failed to unzip dist.zip"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Replace existing dashboard
    rm -rf /data/adb/php7/files/www/zashboard/*
    cp -r dist/* /data/adb/php7/files/www/zashboard/ || {
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

    # Get latest tag
    tag_name=$(/data/user/0/com.termux/files/usr/bin/curl -s https://api.github.com/repos/MetaCubeX/metacubexd/releases/latest | /data/user/0/com.termux/files/usr/bin/jq -r .tag_name)

    # Download release
    /data/user/0/com.termux/files/usr/bin/wget -q --show-progress https://github.com/MetaCubeX/metacubexd/releases/download/$tag_name/compressed-dist.tgz || {
        echo "❌ Failed to download compressed-dist.tgz"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Extract tgz (using tar, not unzip)
    tar -xzf compressed-dist.tgz || {
        echo "❌ Failed to extract compressed-dist.tgz"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Replace existing dashboard
    rm -rf /data/adb/box/clash/dashboard/*
    rm -rf ./compressed-dist.tgz
    cp -r ./* /data/adb/box/clash/dashboard/ || {
        echo "❌ Failed to copy new dashboard files"
        rm -rf "$TMP_DIR"
        return 1
    }

    # Cleanup
    rm -rf "$TMP_DIR"
    echo "✅ MetaCubeXD updated successfully"
}

# Run both updates
update_zashboard
update_metacubexd

echo "🎉 All dashboards updated successfully"
exit 0