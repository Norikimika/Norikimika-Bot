#!/system/bin/sh

input keyevent KEYCODE_WAKEUP
sleep 2
input keyevent 3
sleep 2
input keyevent 3
sleep 2

am force-stop com.termux
sleep 2
monkey -p com.termux -c android.intent.category.LAUNCHER 1
sleep 2
# input tap 350 420
input tap 350 470
sleep 2
input tap 500 840
sleep 2
# input tap 350 420
input tap 350 470
sleep 2
# input tap 350 420
input tap 350 470
sleep 2
input tap 500 840
sleep 2
input keyevent 3

am force-stop com.macrovideo.v380pro
sleep 2
monkey -p com.macrovideo.v380pro -c android.intent.category.LAUNCHER 1
sleep 60
monkey -p com.macrovideo.v380pro -c android.intent.category.LAUNCHER 1
sleep 60
input swipe 270 800 270 200
sleep 2
input tap 270 480
