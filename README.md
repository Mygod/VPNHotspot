# VPN Hotspot

[![Build Status](https://api.travis-ci.org/Mygod/VPNHotspot.svg)](https://travis-ci.org/Mygod/VPNHotspot)
[![API](https://img.shields.io/badge/API-21%2B-brightgreen.svg?style=flat)](https://android-arsenal.com/api?level=21)
[![Releases](https://img.shields.io/github/downloads/Mygod/VPNHotspot/total.svg)](https://github.com/Mygod/VPNHotspot/releases)
[![Codacy Badge](https://api.codacy.com/project/badge/Grade/e70e52b1a58045819b505c09edcae816)](https://www.codacy.com/app/Mygod/VPNHotspot?utm_source=github.com&amp;utm_medium=referral&amp;utm_content=Mygod/VPNHotspot&amp;utm_campaign=Badge_Grade)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Share your VPN connection over hotspot or repeater. (root required)

## Q & A

### Can I use the repeater feature without enabling VPN?

This app is designed for VPN interface as upstreams to simplify handling of connection change and NAT issues.
You could use stub VPN apps like [Adguard](https://github.com/AdguardTeam/AdguardForAndroid)
together with this app as a workaround.

### Can I use this app without root?

Without root, you can only:

* View connected devices for system tethering and monitor them if your SELinux status is not enforcing
  (otherwise you can still do manual refreshes);
* Play around with settings.
