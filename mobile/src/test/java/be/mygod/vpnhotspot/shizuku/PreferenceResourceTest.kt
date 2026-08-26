package be.mygod.vpnhotspot.shizuku

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The transition table of the one owner whose states are decided by a *result code*, checked entry by entry
 * rather than inferred from a generic helper.
 *
 * It is worth a table because the mapping is not the obvious one. `TetheringService` answers
 * `TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` from the permission check, before the mutation is ever
 * posted, and `Tethering` answers `TETHER_ERROR_NO_ERROR` from inside the posted runnable, after it. So a
 * nonzero code is not "the call failed" but "nothing moved", and the debt it leaves behind depends on which
 * direction was being attempted - which is exactly the kind of thing a shared state machine would get wrong
 * by treating both answers as failures.
 */
class PreferenceResourceTest {
    private val preference = PreferenceResource()

    @Test
    fun nothingIsOwedBeforeAnythingIsAsked() {
        assertEquals(ResourceState.ABSENT, preference.state)
        assertFalse(preference.outstanding)
        assertFalse(preference.clearable)
        // Retryable by definition: a clear whose outcome is unknown is reissued by the session's own retained
        // cleanup, and by the next start when that cleanup is still owed, so it must never be terminal.
        assertFalse(preference.terminal)
    }

    /** Issuance is recorded before the IPC, so the flag is never set with nothing owning it. */
    @Test
    fun issuingASetIsOwedImmediately() {
        preference.settingIssued()
        assertEquals(ResourceState.ISSUING, preference.state)
        assertTrue(preference.outstanding)
    }

    /** A denied set proves the flag never moved, so there is nothing left to clear. */
    @Test
    fun aDeniedSetOwesNothing() {
        preference.settingIssued()
        preference.settingDenied()
        assertEquals(ResourceState.ABSENT, preference.state)
        assertFalse(preference.outstanding)
        assertFalse(preference.clearable)
    }

    @Test
    fun aConfirmedSetIsOwedAClear() {
        preference.settingIssued()
        preference.settingMutated()
        assertEquals(ResourceState.LIVE, preference.state)
        assertTrue(preference.clearable)
    }

    /** No answer leaves the handler free to have mutated, so the clear is owed just the same. */
    @Test
    fun anUnansweredSetIsAlsoOwedAClear() {
        preference.settingIssued()
        preference.settingUnknown()
        assertEquals(ResourceState.UNKNOWN, preference.state)
        assertTrue(preference.clearable)
        assertTrue(preference.outstanding)
        assertFalse(preference.terminal)
    }

    @Test
    fun aConfirmedClearEndsTheDebt() {
        preference.settingIssued()
        preference.settingMutated()
        preference.clearingIssued()
        assertEquals(ResourceState.RELEASE_ISSUED, preference.state)
        preference.clearingConfirmed()
        assertEquals(ResourceState.CONFIRMED, preference.state)
        assertFalse(preference.outstanding)
        assertFalse(preference.clearable)
    }

    /**
     * The asymmetry that makes this table worth writing down: the same nonzero code means "no debt" after a
     * set and "the old debt is exactly as it was" after a clear.
     */
    @Test
    fun aDeniedClearRestoresTheDebtItCameFrom() {
        preference.settingIssued()
        preference.settingMutated()
        preference.clearingIssued()
        preference.clearingDenied()
        assertEquals(ResourceState.LIVE, preference.state)
        assertTrue(preference.clearable)
    }

    /** And it restores an *unknown* debt as unknown rather than promoting it to a known one. */
    @Test
    fun aDeniedClearOfAnUnknownDebtStaysUnknown() {
        preference.settingIssued()
        preference.settingUnknown()
        preference.clearingIssued()
        preference.clearingDenied()
        assertEquals(ResourceState.UNKNOWN, preference.state)
        assertTrue(preference.clearable)
    }

    /** Clearing is idempotent, so an unanswered clear may be reissued until something confirms it. */
    @Test
    fun anUnansweredClearMayBeRetried() {
        preference.settingIssued()
        preference.settingMutated()
        preference.clearingIssued()
        preference.clearingUnknown()
        assertEquals(ResourceState.UNKNOWN, preference.state)
        assertTrue(preference.clearable)
        preference.clearingIssued()
        preference.clearingConfirmed()
        assertEquals(ResourceState.CONFIRMED, preference.state)
        assertFalse(preference.outstanding)
    }

    /**
     * The tethering service dying is the one thing that discharges the debt without a call: the flag lives in
     * that process, so a restarted service starts from false.
     *
     * Every arrangement, because the owners that reach this cannot agree on where the debt had got to. The
     * preference call classifies from the process-terminal latch on any failure, so it can arrive here from
     * a set or a clear in flight; the cleanup path discharges when *acquiring* a connector proves the death,
     * where the clear was never issued at all and the state is whatever the last attempt left. `clearable`
     * is asserted alongside, because it is the question those owners ask next to decide whether to reach for
     * a connector again.
     */
    @Test
    fun serviceDeathDischargesEveryDebt() {
        for (arrange in listOf<PreferenceResource.() -> Unit>(
            { },
            { settingIssued() },
            { settingIssued(); settingMutated() },
            { settingIssued(); settingUnknown() },
            { settingIssued(); settingMutated(); clearingIssued() },
            // A clear in flight over a set that was never answered, which is the state a withdrawal retries
            // from and therefore the state its own death lands on.
            { settingIssued(); settingUnknown(); clearingIssued() },
            // A clear that came back unknown, which is exactly what leaves the debt for the cleanup attempt
            // whose connector acquisition then proves the process is gone.
            { settingIssued(); settingMutated(); clearingIssued(); clearingUnknown() },
            // A denial proves that one transaction did not mutate and restores the debt it came from, so
            // these are the states death has to be able to supersede: a result code outranks nothing once
            // the process holding the flag has gone.
            { settingIssued(); settingDenied() },
            { settingIssued(); settingMutated(); clearingIssued(); clearingDenied() },
            { settingIssued(); settingUnknown(); clearingIssued(); clearingDenied() },
        )) {
            val resource = PreferenceResource()
            resource.arrange()
            resource.lostWithService()
            assertEquals(ResourceState.CONFIRMED, resource.state)
            assertFalse(resource.outstanding)
            assertFalse(resource.clearable)
        }
    }

    /**
     * Reaching the same proof twice settles it once, which is what lets the owners above stay independent:
     * the preference call discharges the debt from the latch and then rethrows, and the cleanup path
     * discharges it again on the way out because acquisition may equally have been what proved the death.
     * Neither has to know whether the other already did it.
     */
    @Test
    fun serviceDeathIsIdempotent() {
        preference.settingIssued()
        preference.settingMutated()
        preference.lostWithService()
        preference.lostWithService()
        assertEquals(ResourceState.CONFIRMED, preference.state)
        assertFalse(preference.outstanding)
        assertFalse(preference.clearable)
    }

    /** Every transition the table does not name is refused rather than silently reinterpreted. */
    @Test
    fun disallowedTransitionsAreRefused() {
        for (arrange in listOf<PreferenceResource.() -> Unit>(
            // Setting twice, which would forget the first debt.
            { settingIssued(); settingIssued() },
            // Confirming a set that was never issued.
            { settingMutated() },
            // Clearing before anything was set.
            { clearingIssued() },
            // Answering a clear that is not in flight.
            { settingIssued(); settingMutated(); clearingConfirmed() },
            // Answering a set with a clear's outcome.
            { settingIssued(); clearingDenied() },
            // Clearing an already confirmed clear.
            { settingIssued(); settingMutated(); clearingIssued(); clearingConfirmed(); clearingIssued() },
        )) {
            val resource = PreferenceResource()
            var refused = false
            try {
                resource.arrange()
            } catch (e: IllegalStateException) {
                refused = true
            }
            assertTrue("$resource accepted a transition it should refuse", refused)
        }
    }
}
