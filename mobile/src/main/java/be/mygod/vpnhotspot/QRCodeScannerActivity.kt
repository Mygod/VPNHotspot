package be.mygod.vpnhotspot

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.os.Parcelable
import android.view.LayoutInflater
import android.view.View
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.camera.core.*
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.core.content.ContextCompat
import androidx.lifecycle.LifecycleOwner
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

class QRCodeScannerActivity : AppCompatActivity() {
    
    companion object {
        const val EXTRA_SCAN_RESULT = "scan_result"
        const val REQUEST_CODE_SCAN = 1001
    }
    
    private lateinit var cameraExecutor: ExecutorService
    private var imageCapture: ImageCapture? = null
    private var camera: Camera? = null
    
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        
        // 设置屏幕方向固定为当前方向
        requestedOrientation = resources.configuration.orientation
        
        // 设置窗口为对话框样式
        window.setLayout(
            (resources.displayMetrics.widthPixels * 0.9).toInt(),
            (resources.displayMetrics.heightPixels * 0.8).toInt()
        )
        
        // 创建对话框布局
        val dialogView = LayoutInflater.from(this).inflate(R.layout.dialog_qr_scanner, null)
        setContentView(dialogView)
        
        // 设置取消按钮
        findViewById<android.widget.Button>(R.id.cancelButton).setOnClickListener {
            setResult(Activity.RESULT_CANCELED)
            finish()
        }
        
        // 初始化相机
        cameraExecutor = Executors.newSingleThreadExecutor()
        startCamera()
    }
    
    private fun startCamera() {
        val cameraProviderFuture = ProcessCameraProvider.getInstance(this)
        
        cameraProviderFuture.addListener({
            val cameraProvider: ProcessCameraProvider = cameraProviderFuture.get()
            
            val preview = Preview.Builder()
                .build()
                .also {
                    it.setSurfaceProvider(findViewById<androidx.camera.view.PreviewView>(R.id.viewFinder).surfaceProvider)
                }
            
            imageCapture = ImageCapture.Builder()
                .setCaptureMode(ImageCapture.CAPTURE_MODE_MINIMIZE_LATENCY)
                .build()
            
            val imageAnalyzer = ImageAnalysis.Builder()
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .build()
                .also {
                    it.setAnalyzer(cameraExecutor, QRCodeAnalyzer { qrCode ->
                        // 扫描成功，解析结果
                        val scanResult = parseScanResult(qrCode)
                        val intent = Intent().apply {
                            putExtra(EXTRA_SCAN_RESULT, scanResult)
                        }
                        setResult(Activity.RESULT_OK, intent)
                        finish()
                    })
                }
            
            try {
                cameraProvider.unbindAll()
                camera = cameraProvider.bindToLifecycle(
                    this as LifecycleOwner,
                    CameraSelector.DEFAULT_BACK_CAMERA,
                    preview,
                    imageCapture,
                    imageAnalyzer
                )
            } catch (exc: Exception) {
                Toast.makeText(this, "相机启动失败", Toast.LENGTH_SHORT).show()
                finish()
            }
        }, ContextCompat.getMainExecutor(this))
    }
    
    private fun parseScanResult(content: String): QRScanResult {
        return try {
            // 检查是否是Web后台URL格式 http://ip:port/api_key
            if (content.startsWith("http://")) {
                val url = content.substring(7) // 移除 "http://"
                val parts = url.split("/")
                if (parts.size >= 2) {
                    val hostPort = parts[0].split(":")
                    val apiKey = parts[1]
                    
                    val ip = hostPort[0]
                    val port = if (hostPort.size > 1) hostPort[1].toInt() else 9999
                    
                    QRScanResult(ip, port, apiKey)
                } else {
                    QRScanResult("", 9999, content) // 默认端口9999
                }
            }
            // 检查是否是连接信息格式 vpnhotspot://ip:port?api_key=xxx
            else if (content.startsWith("vpnhotspot://")) {
                val url = content.substring(13) // 移除 "vpnhotspot://"
                val parts = url.split("?")
                if (parts.size == 2) {
                    val hostPort = parts[0].split(":")
                    val params = parts[1].split("&")
                    
                    val ip = hostPort[0]
                    val port = hostPort[1].toInt()
                    var apiKey = ""
                    
                    for (param in params) {
                        val keyValue = param.split("=")
                        if (keyValue.size == 2 && keyValue[0] == "api_key") {
                            apiKey = keyValue[1]
                            break
                        }
                    }
                    
                    QRScanResult(ip, port, apiKey)
                } else {
                    QRScanResult("", 9999, content) // 默认端口9999
                }
            } else {
                // 纯API Key
                QRScanResult("", 9999, content)
            }
        } catch (e: Exception) {
            QRScanResult("", 9999, content)
        }
    }
    
    override fun onDestroy() {
        super.onDestroy()
        cameraExecutor.shutdown()
    }
    
    data class QRScanResult(
        val ip: String,
        val port: Int,
        val apiKey: String
    ) : Parcelable {
        constructor(parcel: android.os.Parcel) : this(
            parcel.readString() ?: "",
            parcel.readInt(),
            parcel.readString() ?: ""
        )

        override fun writeToParcel(parcel: android.os.Parcel, flags: Int) {
            parcel.writeString(ip)
            parcel.writeInt(port)
            parcel.writeString(apiKey)
        }

        override fun describeContents(): Int {
            return 0
        }

        companion object CREATOR : android.os.Parcelable.Creator<QRScanResult> {
            override fun createFromParcel(parcel: android.os.Parcel): QRScanResult {
                return QRScanResult(parcel)
            }

            override fun newArray(size: Int): Array<QRScanResult?> {
                return arrayOfNulls(size)
            }
        }
    }
    
    private class QRCodeAnalyzer(private val onQRCodeDetected: (String) -> Unit) : ImageAnalysis.Analyzer {
        private val scanner = BarcodeScanning.getClient()
        
        @androidx.camera.core.ExperimentalGetImage
        override fun analyze(imageProxy: ImageProxy) {
            val mediaImage = imageProxy.image
            if (mediaImage != null) {
                val image = InputImage.fromMediaImage(mediaImage, imageProxy.imageInfo.rotationDegrees)
                
                scanner.process(image)
                    .addOnSuccessListener { barcodes ->
                        for (barcode in barcodes) {
                            if (barcode.format == Barcode.FORMAT_QR_CODE) {
                                barcode.rawValue?.let { qrCode ->
                                    onQRCodeDetected(qrCode)
                                }
                            }
                        }
                    }
                    .addOnCompleteListener {
                        imageProxy.close()
                    }
            } else {
                imageProxy.close()
            }
        }
    }
} 