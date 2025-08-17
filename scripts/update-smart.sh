#!/system/bin/sh

set -e

TMP_DIR=$(mktemp -d)
cd "$TMP_DIR" || exit 1
ABI=$(getprop ro.product.cpu.abi)

if [ "$ABI" = "arm64-v8a" ]; then
    cores=android-arm64-v8
  elif [ "$ABI" = "armeabi-v7a" ]; then
    cores=android-armv7
fi

# Download build page
/data/user/0/com.termux/files/usr/bin/wget -q --show-progress -O CORE "https://nightly.link/taamarin/smart/workflows/build/Alpha/$cores" || {
  echo "❌ Failed to fetch download page"
  exit 1
}

# Extract .zip URL
urlcore=$(grep -o 'href="[^"]*\.zip' CORE | sed 's/href="//' | head -n 1) || {
  echo "❌ Failed to extract zip URL"
  exit 1
}

# Download .zip file
/data/user/0/com.termux/files/usr/bin/wget -q --show-progress "$urlcore" || {
  echo "❌ Failed to download zip"
  exit 1
}

rm -f CORE

# Unzip & cleanup
unzip -oq $(basename "$urlcore") || {
  echo "❌ Failed to unzip file"
  exit 1
}

rm -f $(basename "$urlcore")
rm -f version.txt

# Extract mihomo binary
file_name=$(find . -type f -name "*.gz" | head -n 1) || {
  echo "❌ .gz file not found"
  exit 1
}

gunzip "$file_name" || {
  echo "❌ Failed to extract .gz file"
  exit 1
}

rm -f "$file_name"

# Move & set permissions
mv mihomo-* /data/adb/box/bin/xclash/mihomo || {
  echo "❌ Failed to move smart binary"
  exit 1
}

chmod 6755 /data/adb/box/bin/xclash/mihomo
sed -i 's/bin_name="sing-box"/bin_name="clash"/' /data/adb/box/settings.ini

# Restart service
/data/adb/box/scripts/box.service restart || {
  echo "❌ Failed to restart box service"
  exit 1
}

# Success message
echo "✅ Core update successfully"
exit 0
