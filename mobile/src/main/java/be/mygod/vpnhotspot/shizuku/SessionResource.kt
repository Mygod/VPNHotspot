package be.mygod.vpnhotspot.shizuku

import android.net.Network
import android.net.NetworkRequest
import android.os.ParcelFileDescriptor

/**
 * The vocabulary the things one Shizuku session owns are described in.
 *
 * The point of a ledger rather than nullable fields is that the two interesting outcomes are *not* success
 * and failure but success and **unknown**. A wrapped Binder transaction whose reply never arrives - or whose
 * epoch changed underneath it - may well have created what it was asked for, and a release whose closing
 * check failed may well have released it; neither answer is available.
 *
 * The names are shared because the states mean the same thing everywhere. The *transitions* are not shared,
 * which is why there is no generic owner below: each resource is acquired by a different mechanism, proves
 * itself with different evidence, and differs in whether a failed release may be reissued at all. A helper
 * that inferred one machine for all of them would have to guess exactly the things this ledger exists to
 * refuse to guess.
 */
internal enum class ResourceState {
    /** Nothing is owed: either nothing was created, or the platform proved nothing was. */
    ABSENT,
    /** A call that can create it has been issued and has not been classified. */
    ISSUING,
    /** It exists, this session owns it, and the exact handle its release needs is in hand. */
    LIVE,
    /** Its release has been issued and has not been confirmed. */
    RELEASE_ISSUED,
    /** Its release is proven. Only now may the reference be dropped. */
    CONFIRMED,
    /**
     * A call was issued and gave no answer this app can act on. The owner object is retained, because a
     * retry - where one is possible at all - needs the exact instance rather than a flag, and because an
     * owner that names nothing releasable is still the record that forbids a successor.
     *
     * Whether this is recoverable is the owner's business, not this enum's: an unknown agent or exact
     * request can only be released by process death, while an unknown preference clear is idempotent and may
     * be retried until it is confirmed.
     */
    UNKNOWN,
}

/**
 * What every owner below has in common and nothing more: a name for a report, a state, and the two questions
 * the session asks of all of them. Every transition is declared by the owner that can actually justify it.
 */
internal sealed class SessionResource(private val name: String) {
    var state = ResourceState.ABSENT
        protected set
    protected fun expect(vararg allowed: ResourceState) {
        check(state in allowed) { "$name is $state" }
    }

    /** True while this session still owes something, which is exactly what forbids a successor. */
    open val outstanding get() = state != ResourceState.ABSENT && state != ResourceState.CONFIRMED

    /**
     * True while nothing this session can still do would clear the debt, so only the process ending will.
     * Reported as such rather than presented as a cleanup that might succeed on a retry.
     */
    open val terminal get() = false

    override fun toString() = "$name($state)"
}

/**
 * The TUN, which is acquired by *return value* transfer: `createTunInterface` builds the descriptor remotely
 * and hands the client a `ParcelFileDescriptor` on the reply. Either the client receives it - in which case
 * it exists, this process holds it, and it is live at once - or the reply is lost, in which case there is no
 * app-held descriptor at all and the transaction's own transfer cleanup closes what was sent. There is
 * therefore deliberately no [ResourceState.ISSUING] and no unknown app descriptor to manufacture.
 */
internal class DescriptorResource : SessionResource("TUN descriptor") {
    var value: ParcelFileDescriptor? = null
        private set

    fun live(descriptor: ParcelFileDescriptor): ParcelFileDescriptor {
        expect(ResourceState.ABSENT)
        state = ResourceState.LIVE
        value = descriptor
        return descriptor
    }

    /** Closing is idempotent, so a close that threw may be reissued. */
    fun closing(): ParcelFileDescriptor? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED)
        if (state == ResourceState.ABSENT || state == ResourceState.CONFIRMED) return null
        state = ResourceState.RELEASE_ISSUED
        return value
    }

    fun confirm() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
        value = null
    }
}

/**
 * The launched child, owned from `ProcessBuilder.start()` rather than from a completed handshake: a process
 * exists the moment it is spawned, and a later readiness failure may leave it holding a duplicate of the TUN.
 *
 * Its release is an exit fence - EOF, SIGTERM, SIGKILL, observed exit - which is idempotent and is exactly
 * what a retry is for, so a fence that failed stays reissuable rather than becoming unknown.
 */
internal class ChildResource : SessionResource("vpnhotspotd") {
    var value: AppUidDaemon.Child? = null
        private set

    fun live(child: AppUidDaemon.Child): AppUidDaemon.Child {
        expect(ResourceState.ABSENT)
        state = ResourceState.LIVE
        value = child
        return child
    }

    fun fencing(): AppUidDaemon.Child? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED)
        if (state == ResourceState.ABSENT || state == ResourceState.CONFIRMED) return null
        state = ResourceState.RELEASE_ISSUED
        return value
    }

    fun confirm() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
        value = null
    }
}

/**
 * The pinned tethering connector, which is not a remotely created resource at all: acquisition delivers a
 * binder this process already had a right to, and what the session owns is its own local death recipient.
 * Unlinking cannot half-happen, so there is no unknown state; each *use* of the connector is separately
 * epoch-bracketed, which is where the privileged question lives.
 */
internal class ConnectorResource : SessionResource("tethering connector") {
    var value: PinnedTetheringConnector? = null
        private set

    fun live(connector: PinnedTetheringConnector): PinnedTetheringConnector {
        expect(ResourceState.ABSENT)
        state = ResourceState.LIVE
        value = connector
        return connector
    }

    fun unlinking(): PinnedTetheringConnector? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED)
        if (state == ResourceState.ABSENT || state == ResourceState.CONFIRMED) return null
        state = ResourceState.RELEASE_ISSUED
        return value
    }

    fun confirm() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
        value = null
    }
}

/**
 * The global `preferTestNetworks` flag: the one piece of system state a session can leave behind, and the
 * only owner here whose transitions are dictated by a *result code* rather than by a handle.
 *
 * `TetheringService` checks `NETWORK_SETTINGS` and answers
 * `TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` **before** it posts anything, and `Tethering` answers
 * `TETHER_ERROR_NO_ERROR` from inside the posted runnable, after the mutation. So the two answers are not
 * merely success and failure: a nonzero code is authoritative proof that nothing moved, and only a zero code
 * paired with an epoch that still holds proves that something did. Anything else - a deadline with no
 * answer, an epoch that changed after issuance - leaves the handler free to have mutated, which is
 * [ResourceState.UNKNOWN].
 *
 * Unlike the agent and the exact request, unknown here is *not* terminal. Clearing is idempotent, so the
 * retained cleanup reissues it under a fresh same-effective-UID epoch until it is confirmed, and the mode's
 * next start retries whatever the last one still owed.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#274
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#2748
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#285
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#3109
 */
internal class PreferenceResource : SessionResource("tethering preference") {
    /**
     * Where a clear in flight came from, so a *denied* clear - which proves no mutation - restores exactly
     * the debt that was there rather than inventing a cleaner one.
     */
    private var clearingFrom = ResourceState.ABSENT

    /** The set transaction is on its way. Recorded before the IPC, never after its result. */
    fun settingIssued() {
        expect(ResourceState.ABSENT)
        state = ResourceState.ISSUING
    }

    /** A nonzero code: the permission check ran before the mutation was posted, so nothing was set. */
    fun settingDenied() {
        expect(ResourceState.ISSUING)
        state = ResourceState.ABSENT
    }

    /** `TETHER_ERROR_NO_ERROR` under an epoch that still holds, which is the only proof it moved. */
    fun settingMutated() {
        expect(ResourceState.ISSUING)
        state = ResourceState.LIVE
    }

    fun settingUnknown() {
        expect(ResourceState.ISSUING)
        state = ResourceState.UNKNOWN
    }

    /** Nothing to clear, and nothing owed: only a set that was proven or unproven owes a clear. */
    val clearable get() = state == ResourceState.LIVE || state == ResourceState.UNKNOWN

    fun clearingIssued() {
        expect(ResourceState.LIVE, ResourceState.UNKNOWN)
        clearingFrom = state
        state = ResourceState.RELEASE_ISSUED
    }

    /** A nonzero code again proves no mutation, so the debt is exactly what it was before the attempt. */
    fun clearingDenied() {
        expect(ResourceState.RELEASE_ISSUED)
        state = clearingFrom
    }

    fun clearingConfirmed() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
    }

    fun clearingUnknown() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.UNKNOWN
    }

    /**
     * The tethering service died, which is positive proof the flag is gone: `mPreferTestNetworks` lives in
     * `UpstreamNetworkMonitor` inside that process and a restarted service starts from `false`. So the debt
     * is discharged rather than pretended to be live - even though the *process* is finished either way,
     * since `TetheringManager` caches the dead connector permanently.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467
     */
    fun lostWithService() {
        state = ResourceState.CONFIRMED
    }
}

/**
 * The exact foreground request, whose release has two halves that fail independently.
 *
 * The remote half is the only one that matters for correctness: ConnectivityService authorizes
 * `releaseNetworkRequest` against the UID stored with the request, so the release has to be the direct
 * privileged call on the retained handle, bracketed by an epoch whose effective UID matches the issuing one.
 * That bracket proves correct-UID Binder acceptance and ordering; it does not prove the service's handler
 * has already run, which is fine, because the operation is idempotent and out-of-order duplicates no-op.
 *
 * The local half is `ConnectivityManager`'s process-static bookkeeping, cleaned by the ordinary
 * `unregisterNetworkCallback` *after* the privileged release is confirmed. That issues a second, app-UID
 * release which ConnectivityService ignores - the request is already being removed, and a wrong-UID or
 * missing-request event is a no-op - while the framework removes its `sCallbacks` entry and tombstones the
 * callback, which is what lets the objects be collected. It can fail on its own, and when it does the remote
 * release stays confirmed while [localCleanup] keeps naming what is still owed.
 */
internal class ExactRequestResource : SessionResource("exact request") {
    var value: ExactRequest? = null
        private set
    /**
     * The callback whose local `ConnectivityManager` bookkeeping is still owed, retained only between a
     * confirmed remote release and a returned ordinary unregister. Never a reason to reissue the remote
     * release, which is already proven.
     */
    var localCleanup: RequestCallback? = null
        private set

    /**
     * Records the request before `requestNetwork` is called, which is the only ordering with no window in
     * which the platform holds something nothing owns.
     */
    fun issuing(request: ExactRequest): ExactRequest {
        expect(ResourceState.ABSENT)
        state = ResourceState.ISSUING
        value = request
        return request
    }

    /**
     * Classifies the registration by the handle the framework wrote back, and by nothing else. A real handle
     * is proof the request exists, whatever the call did afterwards - a failed closing epoch check included,
     * since that invalidates the *answer*, not the request. Null is the absence of an answer rather than
     * proof of absence, so it is unknown and process-terminal.
     */
    fun settle(handle: NetworkRequest?) {
        expect(ResourceState.ISSUING)
        if (handle != null) {
            state = ResourceState.LIVE
        } else {
            state = ResourceState.UNKNOWN
        }
    }

    /** Reissuable: a release whose closing check failed may have been authorized against the wrong UID. */
    fun releasing(): NetworkRequest? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED, ResourceState.UNKNOWN)
        if (state != ResourceState.LIVE && state != ResourceState.RELEASE_ISSUED) return null
        state = ResourceState.RELEASE_ISSUED
        return value?.handle
    }

    /** The privileged release was accepted under a matching UID. The local bookkeeping is now owed. */
    fun released() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
        localCleanup = value?.callback
        value = null
    }

    fun localCleaned() {
        localCleanup = null
    }

    override val outstanding get() = super.outstanding || localCleanup != null
    override val terminal get() = state == ResourceState.UNKNOWN

    override fun toString() = super.toString() + if (localCleanup == null) "" else "+local"
}

/**
 * The app-hosted agent. Registration hands the app its half by argument and the platform's half by return
 * value, so - exactly as with the exact request - ownership begins at issuance and the outcome is decided by
 * reading back what the platform assigned rather than by the shape of a failure.
 *
 * Its release is `unregister()`, which ends the agent's lifecycle by contract and is therefore issued once
 * rather than reissued; what a retry repeats is the *barrier*, not the call.
 */
internal class AgentResource : SessionResource("network agent") {
    var value: RestrictedAgent? = null
        private set

    fun issuing(agent: RestrictedAgent): RestrictedAgent {
        expect(ResourceState.ABSENT)
        state = ResourceState.ISSUING
        value = agent
        return agent
    }

    /**
     * A `Network` read back from the agent means it is registered and locally unregisterable, whatever the
     * call did afterwards. Null after issuance is unknown and process-terminal: the absence of
     * `onNetworkCreated` is not proof that no remote agent exists, so the local network cannot be fenced.
     */
    fun settle(network: Network?) {
        expect(ResourceState.ISSUING)
        if (network != null) {
            state = ResourceState.LIVE
        } else {
            state = ResourceState.UNKNOWN
        }
    }

    /** Once only. A second `unregister()` would be a call the contract does not define. */
    fun unregistering(): RestrictedAgent? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED, ResourceState.UNKNOWN)
        if (state != ResourceState.LIVE) return null
        state = ResourceState.RELEASE_ISSUED
        return value
    }

    /** Retained through the barrier rather than dropped after the call, so a retry can still await it. */
    val awaiting get() = if (state == ResourceState.RELEASE_ISSUED) value else null

    fun confirm() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
        value = null
    }

    override val terminal get() = state == ResourceState.UNKNOWN
}
