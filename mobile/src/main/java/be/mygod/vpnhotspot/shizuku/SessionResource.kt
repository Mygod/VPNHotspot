package be.mygod.vpnhotspot.shizuku

import android.net.Network
import android.net.NetworkRequest

/** UNKNOWN means a Binder mutation may have happened but cannot be proved or safely guessed. */
internal enum class ResourceState {
    ABSENT,
    ISSUING,
    LIVE,
    RELEASE_ISSUED,
    CONFIRMED,
    UNKNOWN,
}

internal sealed class SessionResource(private val name: String) {
    var state = ResourceState.ABSENT
        protected set
    protected fun expect(vararg allowed: ResourceState) {
        check(state in allowed) { "$name is $state" }
    }

    open val outstanding get() = state != ResourceState.ABSENT && state != ResourceState.CONFIRMED
    open val terminal get() = false

    override fun toString() = "$name($state)"
}

internal class SimpleResource<T : Any>(name: String) : SessionResource(name) {
    var value: T? = null
        private set

    fun live(value: T): T {
        expect(ResourceState.ABSENT)
        state = ResourceState.LIVE
        this.value = value
        return value
    }

    fun releasing(): T? {
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

/** Result codes prove mutation or denial; other endings remain clearable UNKNOWN debt. */
internal class PreferenceResource : SessionResource("tethering preference") {
    private var clearingFrom = ResourceState.ABSENT

    fun settingIssued() {
        expect(ResourceState.ABSENT)
        state = ResourceState.ISSUING
    }

    fun settingDenied() {
        expect(ResourceState.ISSUING)
        state = ResourceState.ABSENT
    }

    fun settingMutated() {
        expect(ResourceState.ISSUING)
        state = ResourceState.LIVE
    }

    fun settingUnknown() {
        expect(ResourceState.ISSUING)
        state = ResourceState.UNKNOWN
    }

    val clearable get() = state == ResourceState.LIVE || state == ResourceState.UNKNOWN

    fun clearingIssued() {
        expect(ResourceState.LIVE, ResourceState.UNKNOWN)
        clearingFrom = state
        state = ResourceState.RELEASE_ISSUED
    }

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

    fun lostWithService() {
        state = ResourceState.CONFIRMED
    }
}

/** Tracks privileged remote release separately from app-UID callback bookkeeping cleanup. */
internal class ExactRequestResource : SessionResource("exact request") {
    var value: ExactRequest? = null
        private set
    var localCleanup: RequestCallback? = null
        private set

    fun issuing(request: ExactRequest): ExactRequest {
        expect(ResourceState.ABSENT)
        state = ResourceState.ISSUING
        value = request
        return request
    }

    fun settle(handle: NetworkRequest?) {
        expect(ResourceState.ISSUING)
        if (handle != null) {
            state = ResourceState.LIVE
        } else {
            state = ResourceState.UNKNOWN
        }
    }

    fun expired() {
        expect(ResourceState.LIVE)
        state = ResourceState.CONFIRMED
        value = null
    }

    fun releasing(): NetworkRequest? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED, ResourceState.UNKNOWN)
        if (state != ResourceState.LIVE && state != ResourceState.RELEASE_ISSUED) return null
        state = ResourceState.RELEASE_ISSUED
        return value?.handle
    }

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

/** An issued agent with no read-back Network is process-terminal UNKNOWN debt. */
internal class AgentResource : SessionResource("network agent") {
    var value: RestrictedAgent? = null
        private set

    fun issuing(agent: RestrictedAgent): RestrictedAgent {
        expect(ResourceState.ABSENT)
        state = ResourceState.ISSUING
        value = agent
        return agent
    }

    fun settle(network: Network?) {
        expect(ResourceState.ISSUING)
        if (network != null) {
            state = ResourceState.LIVE
        } else {
            state = ResourceState.UNKNOWN
        }
    }

    fun unregistering(): RestrictedAgent? {
        expect(ResourceState.ABSENT, ResourceState.LIVE, ResourceState.RELEASE_ISSUED,
            ResourceState.CONFIRMED, ResourceState.UNKNOWN)
        if (state != ResourceState.LIVE) return null
        state = ResourceState.RELEASE_ISSUED
        return value
    }

    val awaiting get() = if (state == ResourceState.RELEASE_ISSUED) value else null

    fun confirm() {
        expect(ResourceState.RELEASE_ISSUED)
        state = ResourceState.CONFIRMED
        value = null
    }

    override val terminal get() = state == ResourceState.UNKNOWN
}
