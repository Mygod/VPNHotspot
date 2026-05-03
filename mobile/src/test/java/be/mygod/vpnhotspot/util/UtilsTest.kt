package be.mygod.vpnhotspot.util

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.InetAddress

class UtilsTest {
    @Test
    fun detectsIpv4Bogons() {
        for (address in listOf(
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.0.1",
            "172.16.0.1",
            "192.0.0.8",
            "192.0.0.11",
            "192.0.0.170",
            "192.0.2.1",
            "192.88.99.2",
            "192.168.0.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        )) assertTrue(address, InetAddress.getByName(address).isBogon)
    }

    @Test
    fun detectsIpv4PublicAddresses() {
        for (address in listOf(
            "8.8.8.8",
            "100.128.0.1",
            "192.0.0.9",
            "192.0.0.10",
            "192.31.196.1",
            "192.52.193.1",
            "192.175.48.1",
        )) assertFalse(address, InetAddress.getByName(address).isBogon)
    }

    @Test
    fun detectsIpv6Bogons() {
        for (address in listOf(
            "::",
            "::1",
            "64:ff9b:1::1",
            "100::1",
            "100:0:0:1::1",
            "2001::1",
            "2001:1::4",
            "2001:2::1",
            "2001:10::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        )) assertTrue(address, InetAddress.getByName(address).isBogon)
    }

    @Test
    fun detectsIpv6PublicAddresses() {
        for (address in listOf(
            "64:ff9b::1",
            "2001:1::1",
            "2001:1::2",
            "2001:1::3",
            "2001:3::1",
            "2001:4:112::1",
            "2001:20::1",
            "2001:30::1",
            "2001:4860:4860::8888",
            "2620:4f:8000::1",
        )) assertFalse(address, InetAddress.getByName(address).isBogon)
    }
}
