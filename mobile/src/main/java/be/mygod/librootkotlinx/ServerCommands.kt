package be.mygod.librootkotlinx

import android.os.Parcelable
import androidx.annotation.MainThread
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.parcelize.Parcelize

interface RootCommand<Result : Parcelable?> : Parcelable {
    /**
     * If a throwable was thrown, it will be wrapped in RemoteException only if it implements [Parcelable].
     */
    @MainThread
    suspend fun execute(): Result
}

typealias RootCommandNoResult = RootCommand<Parcelable?>

/**
 * Execute a command and discards its result, even if an exception occurs.
 *
 * If you want to catch exception, use e.g. [RootCommandNoResult] and return null.
 */
interface RootCommandOneWay : Parcelable {
    @MainThread
    suspend fun execute()
}

interface RootCommandChannel<T : Parcelable?> : Parcelable {
    /**
     * The capacity of the channel that is returned by [create] to be used by client.
     * Only [Channel.UNLIMITED] and [Channel.CONFLATED] is supported for now to avoid blocking the entire connection.
     */
    val capacity: Int get() = Channel.UNLIMITED

    @MainThread
    fun create(scope: CoroutineScope): ReceiveChannel<T>
}

@Parcelize
internal data class CancelCommand(val index: Long) : RootCommandOneWay {
    override suspend fun execute() = error("Internal implementation")
}

@Parcelize
internal class Shutdown : Parcelable
