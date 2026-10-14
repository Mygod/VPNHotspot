package be.mygod.vpnhotspot

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.collectLatest
import java.io.IOException

/** One owner for each tethering type; repeated desired state never starts a second request. */
internal class AutoTetheringTarget(
    scope: CoroutineScope,
    private val requestTimeoutMs: Long,
    private val confirmationTimeoutMs: Long,
    private val initialRetryDelayMs: Long,
    private val maximumRetryDelayMs: Long,
    private val startRequest: suspend () -> Unit,
    private val onFailure: (Exception, Long) -> Unit = { _, _ -> },
) {
    private val wanted = MutableStateFlow(false)

    init {
        scope.launch {
            wanted.collectLatest { needed ->
                if (!needed) return@collectLatest
                var delayMs = initialRetryDelayMs
                while (currentCoroutineContext().isActive) {
                    try {
                        if (withTimeoutOrNull(requestTimeoutMs) { startRequest(); true } != true) {
                            throw IOException("Tethering start callback deadline exceeded")
                        }
                        // Keep ownership until the observed interface clears demand. A callback
                        // alone is not proof that tethering became active on vendor frameworks.
                        delay(confirmationTimeoutMs)
                        throw IOException("Tethering interface did not appear after start callback")
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        onFailure(e, delayMs)
                        delay(delayMs)
                        delayMs = (delayMs * 2).coerceAtMost(maximumRetryDelayMs)
                    }
                }
            }
        }
    }

    fun update(needed: Boolean) { wanted.value = needed }
}
