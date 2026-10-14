package be.mygod.vpnhotspot

import kotlinx.coroutines.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AutoTetheringTargetTest {
    @Test fun missingCallbackRetriesWithoutOverlappingRequests() = runBlocking {
        val owner = CoroutineScope(coroutineContext + SupervisorJob())
        var calls = 0
        var active = 0
        var maximum = 0
        try {
            val target = AutoTetheringTarget(owner, 25, 25, 5, 20, {
                calls++
                active++
                maximum = maxOf(maximum, active)
                try { awaitCancellation() } finally { active-- }
            })
            target.update(true)
            delay(100)
            assertTrue("missing callback must not block recovery forever", calls >= 2)
            assertEquals(1, maximum)
            target.update(false)
            delay(15)
            val stopped = calls
            delay(60)
            assertEquals(stopped, calls)
        } finally { owner.cancel() }
    }

    @Test fun successfulCallbackWithoutInterfaceMustRetry() = runBlocking {
        val owner = CoroutineScope(coroutineContext + SupervisorJob())
        var calls = 0
        try {
            val target = AutoTetheringTarget(owner, 25, 25, 5, 20, { calls++ })
            target.update(true)
            delay(100)
            assertTrue("success callback is not an observed tethered interface", calls >= 2)
        } finally { owner.cancel() }
    }

    @Test fun repeatedDemandDoesNotStartAgainWhileAwaitingInterface() = runBlocking {
        val owner = CoroutineScope(coroutineContext + SupervisorJob())
        var calls = 0
        try {
            val target = AutoTetheringTarget(owner, 100, 150, 5, 20, { calls++ })
            target.update(true)
            delay(20)
            repeat(5) { target.update(true) }
            delay(20)
            assertEquals(1, calls)
            target.update(false) // observed interface or disabled auto-start: neither stops the hotspot
            delay(180)
            assertEquals(1, calls)
        } finally { owner.cancel() }
    }
}
