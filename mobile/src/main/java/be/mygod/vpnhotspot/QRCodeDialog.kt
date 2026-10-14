package be.mygod.vpnhotspot

import android.app.Dialog
import android.content.Context
import android.graphics.Bitmap
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.ImageView
import android.widget.TextView
import androidx.fragment.app.DialogFragment
import be.mygod.vpnhotspot.util.QRCodeGenerator

class QRCodeDialog : DialogFragment() {
    
    companion object {
        private const val ARG_CONTENT = "content"
        private const val ARG_TITLE = "title"
        private const val ARG_DESCRIPTION = "description"
        private const val ARG_BITMAP = "bitmap"
        
        fun newInstance(content: String, title: String = "二维码"): QRCodeDialog {
            return QRCodeDialog().apply {
                arguments = Bundle().apply {
                    putString(ARG_CONTENT, content)
                    putString(ARG_TITLE, title)
                }
            }
        }
        
        fun newInstance(bitmap: Bitmap, title: String = "二维码", description: String = ""): QRCodeDialog {
            return QRCodeDialog().apply {
                arguments = Bundle().apply {
                    putParcelable(ARG_BITMAP, bitmap)
                    putString(ARG_TITLE, title)
                    putString(ARG_DESCRIPTION, description)
                }
            }
        }
    }
    
    override fun onCreateDialog(savedInstanceState: Bundle?): Dialog {
        val title = arguments?.getString(ARG_TITLE) ?: "二维码"
        val description = arguments?.getString(ARG_DESCRIPTION) ?: ""
        
        val dialog = Dialog(requireContext())
        dialog.setTitle(title)
        
        val layout = LayoutInflater.from(context).inflate(R.layout.dialog_qr_code, null)
        val imageView = layout.findViewById<ImageView>(R.id.qrCodeImageView)
        val descriptionView = layout.findViewById<TextView>(R.id.descriptionTextView)
        
        // 计算合适的二维码尺寸
        val displayMetrics = requireContext().resources.displayMetrics
        val screenWidth = displayMetrics.widthPixels
        val screenHeight = displayMetrics.heightPixels
        val maxSize = minOf(screenWidth, screenHeight) - 100 // 留出边距
        
        // 设置二维码图片
        val bitmap = arguments?.getParcelable<Bitmap>(ARG_BITMAP)
        if (bitmap != null) {
            imageView.setImageBitmap(bitmap)
        } else {
            val content = arguments?.getString(ARG_CONTENT) ?: ""
            val qrCodeBitmap = QRCodeGenerator.generateQRCode(content, maxSize)
            imageView.setImageBitmap(qrCodeBitmap)
        }
        
        // 设置描述信息
        if (description.isNotEmpty()) {
            descriptionView.text = description
            descriptionView.visibility = View.VISIBLE
        } else {
            descriptionView.visibility = View.GONE
        }
        
        dialog.setContentView(layout)
        return dialog
    }
    
    override fun onCreateView(
        inflater: LayoutInflater,
        container: ViewGroup?,
        savedInstanceState: Bundle?
    ): View? {
        val layout = inflater.inflate(R.layout.dialog_qr_code, container, false)
        val imageView = layout.findViewById<ImageView>(R.id.qrCodeImageView)
        val descriptionView = layout.findViewById<TextView>(R.id.descriptionTextView)
        
        // 计算合适的二维码尺寸
        val displayMetrics = requireContext().resources.displayMetrics
        val screenWidth = displayMetrics.widthPixels
        val screenHeight = displayMetrics.heightPixels
        val maxSize = minOf(screenWidth, screenHeight) - 100 // 留出边距
        
        // 设置二维码图片
        val bitmap = arguments?.getParcelable<Bitmap>(ARG_BITMAP)
        if (bitmap != null) {
            imageView.setImageBitmap(bitmap)
        } else {
            val content = arguments?.getString(ARG_CONTENT) ?: ""
            val qrCodeBitmap = QRCodeGenerator.generateQRCode(content, maxSize)
            imageView.setImageBitmap(qrCodeBitmap)
        }
        
        // 设置描述信息
        val description = arguments?.getString(ARG_DESCRIPTION) ?: ""
        if (description.isNotEmpty()) {
            descriptionView.text = description
            descriptionView.visibility = View.VISIBLE
        } else {
            descriptionView.visibility = View.GONE
        }
        
        return layout
    }
} 