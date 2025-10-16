#!/system/bin/sh

set -e

TMP_DIR=$(mktemp -d)
cd "$TMP_DIR" || exit 1
ABI=$(getprop ro.product.cpu.abi)

if [ "$ABI" = "arm64-v8a" ]; then
    cores=android-arm64
  elif [ "$ABI" = "armeabi-v7a" ]; then
    cores=android-armv7
fi

# Download build page
/data/user/0/com.termux/files/usr/bin/wget -q --show-progress -O CORE "https://nightly.link/taamarin/sing-box/workflows/tw/dev-next" || {
  echo "❌ Failed to fetch download page"
  exit 1
}

# Extract .zip URL
urlcore=$(grep -o "href=\"[^\"]*sing-box-[^\"]*-$cores.zip" CORE | sed 's/href="//' | head -n 1) || {
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
rm -f tags.txt

# Extract mihomo binary
file_name=$(find . -type f -name "*.tar.gz" | head -n 1) || {
  echo "❌ .tar.gz file not found"
  exit 1
}

tar -xzf "$file_name" || {
  echo "❌ Failed to extract .tar.gz file"
  exit 1
}

rm -f "$file_name"

# Move & set permissions
mv bin/sing-box-* /data/adb/box/bin/sing-box || {
  echo "❌ Failed to move sing-box binary"
  exit 1
}

chmod 755 /data/adb/box/bin/sing-box
sed -i 's/bin_name="clash"/bin_name="sing-box"/' /data/adb/box/settings.ini

# Restart service
/data/adb/box/scripts/box.service restart || {
  echo "❌ Failed to restart box service"
  exit 1
}

# Success message
echo "✅ Core update successfully"
exit 0
