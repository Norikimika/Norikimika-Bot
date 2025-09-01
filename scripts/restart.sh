#!/system/bin/sh

# Wake up the device and go to home screen
input keyevent KEYCODE_WAKEUP
sleep 2
input keyevent 3
sleep 2
input keyevent 3
sleep 2

# Server
monkey -p com.termux -c android.intent.category.LAUNCHER 1
sleep 2
input tap 687 1300
sleep 2
input tap 970 2180
sleep 2
input keyevent 3
sleep 2
