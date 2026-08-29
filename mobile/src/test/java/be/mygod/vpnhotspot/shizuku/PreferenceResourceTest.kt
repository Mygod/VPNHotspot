package be.mygod.vpnhotspot.shizuku

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PreferenceResourceTest {
    @Test
    fun setOutcomesClassifyTheDebt() {
        val denied = PreferenceResource()
        denied.settingIssued()
        assertEquals(ResourceState.ISSUING, denied.state)
        denied.settingDenied()
        assertEquals(ResourceState.ABSENT, denied.state)
        assertFalse(denied.outstanding)

        val mutated = PreferenceResource()
        mutated.settingIssued()
        mutated.settingMutated()
        assertEquals(ResourceState.LIVE, mutated.state)
        assertTrue(mutated.clearable)

        val unknown = PreferenceResource()
        unknown.settingIssued()
        unknown.settingUnknown()
        assertEquals(ResourceState.UNKNOWN, unknown.state)
        assertTrue(unknown.outstanding)
        assertTrue(unknown.clearable)
        assertFalse(unknown.terminal)
    }

    @Test
    fun clearDenialRestoresThePreviousDebt() {
        for (unknown in listOf(false, true)) {
            val resource = PreferenceResource()
            resource.settingIssued()
            if (unknown) resource.settingUnknown() else resource.settingMutated()
            resource.clearingIssued()
            resource.clearingDenied()
            assertEquals(if (unknown) ResourceState.UNKNOWN else ResourceState.LIVE, resource.state)
            assertTrue(resource.clearable)
        }
    }

    @Test
    fun unknownClearCanBeRetriedToConfirmation() {
        val resource = PreferenceResource()
        resource.settingIssued()
        resource.settingMutated()
        resource.clearingIssued()
        resource.clearingUnknown()
        assertEquals(ResourceState.UNKNOWN, resource.state)
        assertTrue(resource.clearable)

        resource.clearingIssued()
        resource.clearingConfirmed()
        assertEquals(ResourceState.CONFIRMED, resource.state)
        assertFalse(resource.outstanding)
        assertFalse(resource.clearable)
    }

    @Test
    fun serviceDeathDischargesEveryDebtIdempotently() {
        for (arrange in listOf<PreferenceResource.() -> Unit>(
            { },
            { settingIssued() },
            { settingIssued(); settingMutated() },
            { settingIssued(); settingUnknown() },
            { settingIssued(); settingMutated(); clearingIssued() },
            { settingIssued(); settingUnknown(); clearingIssued() },
            { settingIssued(); settingMutated(); clearingIssued(); clearingUnknown() },
            { settingIssued(); settingDenied() },
            { settingIssued(); settingMutated(); clearingIssued(); clearingDenied() },
            { settingIssued(); settingUnknown(); clearingIssued(); clearingDenied() },
        )) {
            val resource = PreferenceResource()
            resource.arrange()
            resource.lostWithService()
            resource.lostWithService()
            assertEquals(ResourceState.CONFIRMED, resource.state)
            assertFalse(resource.outstanding)
            assertFalse(resource.clearable)
        }
    }
}
