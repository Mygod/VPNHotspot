package be.mygod.vpnhotspot.util

import android.graphics.Bitmap
import android.graphics.Color
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import java.util.*

/**
 * 二维码生成工具类
 */
object QRCodeGenerator {
    
    /**
     * 生成API Key二维码
     */
    fun generateApiKeyQRCode(apiKey: String, size: Int = 512): Bitmap {
        val hints = EnumMap<EncodeHintType, Any>(EncodeHintType::class.java)
        hints[EncodeHintType.CHARACTER_SET] = "UTF-8"
        hints[EncodeHintType.MARGIN] = 1
        
        val writer = QRCodeWriter()
        val bitMatrix = writer.encode(apiKey, BarcodeFormat.QR_CODE, size, size, hints)
        
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.RGB_565)
        for (x in 0 until size) {
            for (y in 0 until size) {
                bitmap.setPixel(x, y, if (bitMatrix[x, y]) Color.BLACK else Color.WHITE)
            }
        }
        
        return bitmap
    }
    
    /**
     * 生成连接信息二维码（包含IP、端口、API Key）
     */
    fun generateConnectionQRCode(ip: String, port: Int, apiKey: String, size: Int = 512): Bitmap {
        val connectionInfo = "vpnhotspot://$ip:$port?api_key=$apiKey"
        return generateQRCode(connectionInfo, size)
    }
    
    /**
     * 生成Web后台访问二维码（包含完整URL）
     */
    fun generateWebAccessQRCode(ip: String, port: Int, apiKey: String, size: Int = 512): Bitmap {
        val webUrl = "http://$ip:$port/$apiKey"
        return generateQRCode(webUrl, size)
    }
    
    /**
     * 生成通用二维码
     */
    fun generateQRCode(content: String, size: Int = 512): Bitmap {
        val hints = EnumMap<EncodeHintType, Any>(EncodeHintType::class.java)
        hints[EncodeHintType.CHARACTER_SET] = "UTF-8"
        hints[EncodeHintType.MARGIN] = 1
        
        val writer = QRCodeWriter()
        val bitMatrix = writer.encode(content, BarcodeFormat.QR_CODE, size, size, hints)
        
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.RGB_565)
        for (x in 0 until size) {
            for (y in 0 until size) {
                bitmap.setPixel(x, y, if (bitMatrix[x, y]) Color.BLACK else Color.WHITE)
            }
        }
        
        return bitmap
    }
} 