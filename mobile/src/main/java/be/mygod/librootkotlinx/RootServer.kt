package be.mygod.librootkotlinx

import android.content.Context
import android.os.Build
import android.os.Looper
import android.os.Parcelable
import android.os.RemoteException
import android.system.Os
import android.system.OsConstants
import androidx.collection.LongSparseArray
import androidx.collection.set
import androidx.collection.valueIterator
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.*
import java.util.*
import java.util.concurrent.CountDownLatch
import kotlin.system.exitProcess

class RootServer {
    private sealed class Callback(private val server: RootServer, private val index: Long,
                                  protected val classLoader: ClassLoader?) {
        var active = true

        abstract fun cancel()
        abstract fun shouldRemove(result: Byte): Boolean
        abstract operator fun invoke(input: DataInputStream, result: Byte)
        suspend fun sendClosed() = withContext(NonCancellable) { server.execute(CancelCommand(index)) }

        private fun initException(targetClass: Class<*>, message: String): Throwable {
            @Suppress("NAME_SHADOWING")
            var targetClass = targetClass
            while (true) {
                try {
                    // try to find a message constructor
                    return targetClass.getDeclaredConstructor(String::class.java).newInstance(message) as Throwable
                } catch (_: ReflectiveOperationException) { }
                targetClass = targetClass.superclass
            }
        }
        private fun makeRemoteException(cause: Throwable, message: String? = null) =
                if (cause is CancellationException) cause else RemoteException(message).initCause(cause)
        protected fun DataInputStream.readException(result: Byte) = when (result.toInt()) {
            EX_GENERIC -> {
                val message = readUTF()
                val name = message.split(':', limit = 2)[0]
                makeRemoteException(initException(try {
                    classLoader?.loadClass(name)
                } catch (_: ClassNotFoundException) {
                    null
                } ?: Class.forName(name), message), message)
            }
            EX_PARCELABLE -> makeRemoteException(readParcelable<Parcelable>(classLoader) as Throwable)
            EX_SERIALIZABLE -> makeRemoteException(readSerializable(classLoader) as Throwable)
            else -> throw IllegalArgumentException("Unexpected result $result")
        }

        class Ordinary(server: RootServer, index: Long, classLoader: ClassLoader?,
                       private val callback: CompletableDeferred<Parcelable?>) : Callback(server, index, classLoader) {
            override fun cancel() = callback.cancel()
            override fun shouldRemove(result: Byte) = true
            override fun invoke(input: DataInputStream, result: Byte) {
                if (result.toInt() == SUCCESS) callback.complete(input.readParcelable(classLoader))
                else callback.completeExceptionally(input.readException(result))
            }
        }

        class Channel(server: RootServer, index: Long, classLoader: ClassLoader?,
                      private val channel: SendChannel<Parcelable?>) : Callback(server, index, classLoader) {
            val finish: CompletableDeferred<Unit> = CompletableDeferred()
            override fun cancel() = finish.cancel()
            override fun shouldRemove(result: Byte) = result.toInt() != SUCCESS
            override fun invoke(input: DataInputStream, result: Byte) {
                when (result.toInt()) {
                    SUCCESS -> channel.trySend(input.readParcelable(classLoader)).onClosed {
                        active = false
                        GlobalScope.launch(Dispatchers.Unconfined) { sendClosed() }
                        finish.completeExceptionally(it
                            ?: ClosedSendChannelException("Channel was closed normally"))
                        return
                    }.onFailure { throw it!! }  // the channel we are supporting should never block
                    CHANNEL_CONSUMED -> finish.complete(Unit)
                    else -> finish.completeExceptionally(input.readException(result))
                }
            }
        }
    }

    class UnexpectedExitException : RemoteException("Root process exited unexpectedly")

    private lateinit var process: Process
    /**
     * Thread safety: needs to be protected by mutex.
     */
    private lateinit var output: DataOutputStream

    @Volatile
    var active = false
    private var counter = 0L
    private lateinit var callbackListenerExit: Deferred<Unit>
    private val callbackLookup = LongSparseArray<Callback>()
    private val mutex = Mutex()

    private fun readUnexpectedStderr(): String? {
        if (!this::process.isInitialized) return null
        var available = process.errorStream.available()
        return if (available <= 0) null else String(ByteArrayOutputStream().apply {
            while (available > 0) {
                val bytes = ByteArray(available)
                val len = process.errorStream.read(bytes)
                if (len < 0) throw EOFException()   // should not happen
                write(bytes, 0, len)
                available = process.errorStream.available()
            }
        }.toByteArray())
    }

    private fun BufferedReader.lookForToken(token: String) {
        while (true) {
            val line = readLine() ?: throw EOFException()
            if (line.endsWith(token)) {
                val extraLength = line.length - token.length
                if (extraLength > 0) Logger.me.w(line.substring(0, extraLength))
                break
            }
            Logger.me.w(line)
        }
    }
    private fun doInit(context: Context, niceName: String) {
        val (reader, writer) = try {
            process = ProcessBuilder("su").start()
            val token1 = UUID.randomUUID().toString()
            val writer = DataOutputStream(process.outputStream.buffered())
            writer.writeBytes("echo $token1\n")
            writer.flush()
            val reader = process.inputStream.bufferedReader()
            reader.lookForToken(token1)
            Logger.me.d("Root shell initialized")
            reader to writer
        } catch (e: Exception) {
            throw NoShellException(e)
        }
        try {
            val token2 = UUID.randomUUID().toString()
            val persistence = File(context.codeCacheDir, ".librootkotlinx-uuid")
            val uuid = context.packageName + '@' + if (persistence.canRead()) persistence.readText() else {
                UUID.randomUUID().toString().also { persistence.writeText(it) }
            }
            val (script, relocated) = AppProcess.relocateScript(uuid)
            script.appendLine(AppProcess.launchString(context.packageCodePath, RootServer::class.java.name, relocated,
                    niceName) + " $token2")
            writer.writeBytes(script.toString())
            writer.flush()
            reader.lookForToken(token2) // wait for ready signal
        } catch (e: Exception) {
            throw RuntimeException("Failed to launch root daemon", e)
        }
        output = writer
        require(!active)
        active = true
        Logger.me.d("Root server initialized")
    }

    private fun callbackSpin() {
        val input = DataInputStream(process.inputStream.buffered())
        while (active) {
            val index = try {
                input.readLong()
            } catch (_: EOFException) {
                break
            }
            val result = input.readByte()
            val callback = mutex.synchronized {
                if (active) (callbackLookup[index] ?: error("Empty callback #$index")).also {
                    if (it.shouldRemove(result)) {
                        callbackLookup.remove(index)
                        it.active = false
                    }
                } else null
            } ?: break
            Logger.me.d("Received callback #$index: $result")
            callback(input, result)
        }
    }

    /**
     * Initialize a RootServer synchronously, can throw a lot of exceptions.
     *
     * @param context Any [Context] from the app.
     * @param niceName Name to call the rooted Java process.
     */
    suspend fun init(context: Context, niceName: String = "${context.packageName}:root") = try {
        val future = CompletableDeferred<Unit>()
        callbackListenerExit = GlobalScope.async(Dispatchers.IO) {
            try {
                doInit(context, niceName)
                future.complete(Unit)
            } catch (e: Throwable) {
                future.completeExceptionally(e)
                return@async
            }
            val errorReader = async(Dispatchers.IO) {
                try {
                    process.errorStream.bufferedReader().forEachLine(Logger.me::w)
                } catch (_: IOException) { }
            }
            try {
                callbackSpin()
                if (active) throw UnexpectedExitException()
            } catch (e: Throwable) {
                process.destroy()
                throw e
            } finally {
                Logger.me.d("Waiting for exit")
                errorReader.await()
                process.waitFor()
                withContext(NonCancellable) { closeInternal(true) }
            }
        }
        future.await()
    } finally {
        try {
            readUnexpectedStderr()?.let { Logger.me.e(it) }
        } catch (e: IOException) {
            Logger.me.e("Failed to read from stderr", e)    // avoid the real exception being swallowed
        }
    }

    /**
     * Caller should check for active.
     */
    private fun sendLocked(command: Parcelable) {
        output.writeParcelable(command)
        output.flush()
        Logger.me.d("Sent #$counter: $command")
        counter++
    }

    suspend fun execute(command: RootCommandOneWay) = mutex.withLock { if (active) sendLocked(command) }
    @Throws(RemoteException::class)
    suspend inline fun <reified T : Parcelable?> execute(command: RootCommand<T>) =
        execute(command, T::class.java.classLoader)
    @Throws(RemoteException::class)
    suspend fun <T : Parcelable?> execute(command: RootCommand<T>, classLoader: ClassLoader?): T {
        val future = CompletableDeferred<T>()
        @Suppress("UNCHECKED_CAST")
        val callback = Callback.Ordinary(this, counter, classLoader, future as CompletableDeferred<Parcelable?>)
        mutex.withLock {
            if (active) {
                callbackLookup[counter] = callback
                sendLocked(command)
            } else future.cancel()
        }
        try {
            return future.await()
        } finally {
            if (callback.active) callback.sendClosed()
            callback.active = false
        }
    }

    @ExperimentalCoroutinesApi
    @Throws(RemoteException::class)
    inline fun <reified T : Parcelable?> create(command: RootCommandChannel<T>, scope: CoroutineScope) =
            create(command, scope, T::class.java.classLoader)
    @ExperimentalCoroutinesApi
    @Throws(RemoteException::class)
    fun <T : Parcelable?> create(command: RootCommandChannel<T>, scope: CoroutineScope,
                                 classLoader: ClassLoader?) = scope.produce<T>(
            SupervisorJob(), command.capacity.also {
                when (it) {
                    Channel.UNLIMITED, Channel.CONFLATED -> { }
                    else -> throw IllegalArgumentException("Unsupported channel capacity $it")
                }
            }) {
        @Suppress("UNCHECKED_CAST")
        val callback = Callback.Channel(this@RootServer, counter, classLoader, this as SendChannel<Parcelable?>)
        mutex.withLock {
            if (active) {
                callbackLookup[counter] = callback
                sendLocked(command)
            } else callback.finish.cancel()
        }
        try {
            callback.finish.await()
        } finally {
            if (callback.active) callback.sendClosed()
            callback.active = false
        }
    }

    private suspend fun closeInternal(fromWorker: Boolean = false) = mutex.withLock {
        if (active) {
            active = false
            Logger.me.d(if (fromWorker) "Shutting down from worker" else "Shutting down from client")
            try {
                sendLocked(Shutdown())
                output.close()
                process.outputStream.close()
            } catch (e: IOException) {
                if (!e.isEBADF) Logger.me.w("send Shutdown failed", e)
            }
            Logger.me.d("Client closed")
        }
        if (fromWorker) {
            for (callback in callbackLookup.valueIterator()) callback.cancel()
            callbackLookup.clear()
        }
    }
    /**
     * Shutdown the instance gracefully.
     */
    suspend fun close() {
        closeInternal()
        try {
            withTimeout(10000) { callbackListenerExit.await() }
        } catch (e: TimeoutCancellationException) {
            Logger.me.w("Closing the instance has timed out", e)
            if (Build.VERSION.SDK_INT < 26) process.destroy() else if (process.isAlive) process.destroyForcibly()
        } catch (e: UnexpectedExitException) {
            Logger.me.w(e.message)
        }
    }

    companion object {
        private const val SUCCESS = 0
        private const val EX_GENERIC = 1
        private const val EX_PARCELABLE = 2
        private const val EX_SERIALIZABLE = 4
        private const val CHANNEL_CONSUMED = 3

        private fun DataInputStream.readByteArray() = ByteArray(readInt()).also { readFully(it) }

        private inline fun <reified T : Parcelable> DataInputStream.readParcelable(
            classLoader: ClassLoader? = T::class.java.classLoader) = readByteArray().toParcelable<T>(classLoader)
        private fun DataOutputStream.writeParcelable(data: Parcelable?, parcelableFlags: Int = 0) {
            val bytes = data.toByteArray(parcelableFlags)
            writeInt(bytes.size)
            write(bytes)
        }

        private fun DataInputStream.readSerializable(classLoader: ClassLoader?) =
                object : ObjectInputStream(ByteArrayInputStream(readByteArray())) {
                    override fun resolveClass(desc: ObjectStreamClass) = Class.forName(desc.name, false, classLoader)
                }.readObject()

        private inline fun <T> Mutex.synchronized(crossinline block: () -> T): T = runBlocking {
            withLock { block() }
        }

        @JvmStatic
        fun main(args: Array<String>) {
            Thread.setDefaultUncaughtExceptionHandler { thread, throwable ->
                Logger.me.e("Uncaught exception from $thread", throwable)
                throwable.printStackTrace()     // stderr will be read by listener
                exitProcess(1)
            }
            rootMain(args)
            exitProcess(0)  // there might be other non-daemon threads
        }

        private fun DataOutputStream.pushThrowable(callback: Long, e: Throwable) {
            writeLong(callback)
            if (e is Parcelable) {
                writeByte(EX_PARCELABLE)
                writeParcelable(e)
            } else try {
                val bytes = ByteArrayOutputStream().apply {
                    ObjectOutputStream(this).use { it.writeObject(e) }
                }.toByteArray()
                writeByte(EX_SERIALIZABLE)
                writeInt(bytes.size)
                write(bytes)
            } catch (_: NotSerializableException) {
                writeByte(EX_GENERIC)
                writeUTF(e.stackTraceToString())
            }
            flush()
        }
        private fun DataOutputStream.pushResult(callback: Long, result: Parcelable?) {
            writeLong(callback)
            writeByte(SUCCESS)
            writeParcelable(result)
            flush()
        }

        private fun rootMain(args: Array<String>) {
            require(args.isNotEmpty())
            val mainInitialized = CountDownLatch(1)
            val main = Thread({
                @Suppress("DEPRECATION")
                Looper.prepareMainLooper()
                mainInitialized.countDown()
                Looper.loop()
            }, "main")
            main.start()
            val job = Job()
            val defaultWorker by lazy {
                mainInitialized.await()
                CoroutineScope(Dispatchers.Main.immediate + job)
            }
            val callbackWorker = newSingleThreadContext("callbackWorker")
            val cancellables = LongSparseArray<() -> Unit>()

            // thread safety: usage of output should be guarded by callbackWorker
            val output = DataOutputStream(FileOutputStream(Os.dup(FileDescriptor.out)).buffered().apply {
                // prevent future write attempts to System.out, possibly from Samsung changes (again)
                Os.dup2(FileDescriptor.err, OsConstants.STDOUT_FILENO)
                System.setOut(System.err)
                val writer = writer()
                writer.appendLine(args[0])  // echo ready signal
                writer.flush()
            })
            // thread safety: usage of input should be in main thread
            val input = DataInputStream(System.`in`.buffered())
            var counter = 0L
            Logger.me.d("Server entering main loop")
            loop@ while (true) {
                val command = try {
                    input.readParcelable<Parcelable>(RootServer::class.java.classLoader)
                } catch (_: EOFException) {
                    break
                }
                val callback = counter
                Logger.me.d("Received #$callback: $command")
                when (command) {
                    is CancelCommand -> cancellables[command.index]?.invoke()
                    is RootCommandOneWay -> defaultWorker.launch {
                        try {
                            command.execute()
                        } catch (e: Throwable) {
                            Logger.me.e("Unexpected exception in RootCommandOneWay ($command.javaClass.simpleName)", e)
                        }
                    }
                    is RootCommand<*> -> {
                        val commandJob = Job()
                        cancellables[callback] = { commandJob.cancel() }
                        defaultWorker.launch(commandJob) {
                            val result = try {
                                val result = command.execute();
                                { output.pushResult(callback, result) }
                            } catch (e: Throwable) {
                                val worker = { output.pushThrowable(callback, e) }
                                worker
                            } finally {
                                cancellables.remove(callback)
                            }
                            withContext(callbackWorker + NonCancellable) { result() }
                        }
                    }
                    is RootCommandChannel<*> -> defaultWorker.launch {
                        val result = try {
                            coroutineScope {
                                command.create(this).also {
                                    cancellables[callback] = { it.cancel() }
                                }.consumeEach { result ->
                                    withContext(callbackWorker) { output.pushResult(callback, result) }
                                }
                            };
                            @Suppress("BlockingMethodInNonBlockingContext") {
                                output.writeByte(CHANNEL_CONSUMED)
                                output.writeLong(callback)
                                output.flush()
                            }
                        } catch (e: Throwable) {
                            val worker = { output.pushThrowable(callback, e) }
                            worker
                        } finally {
                            cancellables.remove(callback)
                        }
                        withContext(callbackWorker + NonCancellable) { result() }
                    }
                    is Shutdown -> break@loop
                    else -> throw IllegalArgumentException("Unrecognized input: $command")
                }
                counter++
            }
            job.cancel()
            Logger.me.d("Clean up initiated before exit. Jobs: ${job.children.joinToString()}")
            if (runBlocking { withTimeoutOrNull(5000) { job.join() } } == null) {
                Logger.me.w("Clean up timeout: ${job.children.joinToString()}")
            } else Logger.me.d("Clean up finished, exiting")
        }
    }
}
