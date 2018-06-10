# VPN Hotspot

[![Build Status](https://api.travis-ci.org/Mygod/VPNHotspot.svg)](https://travis-ci.org/Mygod/VPNHotspot)
[![API](https://img.shields.io/badge/API-21%2B-brightgreen.svg?style=flat)](https://android-arsenal.com/api?level=21)
[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![Codacy Badge](https://api.codacy.com/project/badge/Grade/e70e52b1a58045819b505c09edcae816)](https://www.codacy.com/app/Mygod/VPNHotspot?utm_source=github.com&amp;utm_medium=referral&amp;utm_content=Mygod/VPNHotspot&amp;utm_campaign=Badge_Grade)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Connecting things to your VPN made simple. Share your VPN connection over hotspot or repeater. (**root required**)  
<a href="https://play.google.com/store/apps/details?id=be.mygod.vpnhotspot" target="_blank"><img src="https://play.google.com/intl/en_us/badges/images/generic/en-play-badge.png" height="60"></a>
or <a href="https://labs.xda-developers.com/store/app/be.mygod.vpnhotspot" target="_blank">XDA Labs</a>

This app is useful for:

* Connecting things that don't support VPN like Chromecasts behind corporate firewalls;
* Setting up [gapps](https://support.google.com/pixelphone/answer/7158475) behind corporate firewalls;
* Connecting to your mobile hotspot but you're not bothered to set up VPN on your device;
* Bypassing tethering limits. (you might need more of a real VPN than an ad-blocker to fool a smarter cellular provider)

P.S. You can also do the similar on [Windows](https://www.expressvpn.com/support/vpn-setup/share-vpn-connection-windows/)
and [Mac](https://www.expressvpn.com/support/vpn-setup/share-vpn-connection-mac/).
I don't know about you but I can't get my stupid Windows 10 to work with
[hosted network](https://msdn.microsoft.com/en-us/library/windows/desktop/dd815243(v=vs.85).aspx)
now that they introduced this
[Mobile hotspot](https://support.microsoft.com/en-us/help/4027762/windows-use-your-pc-as-a-mobile-hotspot).

This app is designed to do only minimal changes to your system, so there's almost no chance you will brick your device
and/or break your Internet using this app *under normal conditions*. However there's also absolutely no guarantee it won't.

## Q & A

### Failed to create group due to internal error/repeater shuts down after a while?

This could caused by the Wi-Fi channel you selected is no longer available, due to:

1. Your device doesn't support operating on this channel, or
2. There is some nearby Wi-Fi direct device that broadcasted that it can't operate on the channel you picked.

For maximum stability, you need to set channel = 0 so that your device will pick a channel automatically.
You can also use WPS to connect your 2.4GHz-only device to force the repeater to switch from 5GHz to 2.4GHz for this time.

### [IPv6 tethering?](https://github.com/Mygod/VPNHotspot/issues/6)

### Missing `android.permission.MANAGE_USB` permission?

Toggling USB tethering only works if you install this app as a system app (`/system/priv-app`).
Alternatively, use the toggle in your system settings instead.

### No root?

Without root, you can only:

* View connected devices for system tethering and monitor them;
* Toggle tether switches if you can't do it already;
* Play around with settings and the user interface in general;
* Alternatively you can use try these apps (requires manual proxy configuration or client apps) for normal repeater
  tethering/bypassing tethering limits: (note: these apps are neither free nor open source)
  * [PdaNet+](https://play.google.com/store/apps/details?id=com.pdanet)
  * [NetShare-no-root-tethering](https://play.google.com/store/apps/details?id=kha.prog.mikrotik)
