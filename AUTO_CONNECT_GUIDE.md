# VPNHotspot 自动连接功能指南

## 功能概述
自动连接功能允许用户在进入远程控制页面时，自动尝试连接上次保存的远程设备。

## 工作原理

### SharedPreferences 使用
- **设置存储**: SettingsPreferenceFragment 使用 `App.app.pref` (通过 SharedPreferenceDataStore)
- **设置读取**: RemoteControlFragment 使用 `App.app.pref` (统一使用 App 类中的默认 SharedPreferences)
- **设置键值**: `remote.control.auto.connect` (布尔值，默认 false)

### 数据流程
1. 用户在设置页面开启/关闭"远程控制自动连接"开关
2. 设置值保存在 App.app.pref 中
3. 当用户进入 RemoteControlFragment 时：
   - 在 onResume() 中重新检查设置
   - 读取 `remote.control.auto.connect` 值
   - 如果为 true 且有保存的连接信息，则自动连接

## 测试步骤

### 1. 基本测试
```bash
# 运行测试脚本
./test_auto_connect_functionality.sh

# 或者手动检查
adb shell am start -n be.mygod.vpnhotspot/.MainActivity
```

### 2. 功能验证
1. **打开设置**: 进入 VPNHotspot 设置页面
2. **开启自动连接**: 找到"远程控制自动连接"开关并开启
3. **保存连接信息**: 在远程控制页面手动连接一次设备（保存IP、端口、API Key）
4. **重新进入**: 返回主页面，再次进入远程控制页面
5. **验证自动连接**: 观察是否自动开始连接

### 3. 日志检查
```bash
# 查看实时日志
adb logcat | grep -E "(RemoteControl|Settings|autoConnect)"

# 检查关键日志
# 设置变更: "Settings: 远程控制自动连接已设置为 true/false"
# 读取设置: "RemoteControl: autoConnectEnabled = true/false"
# 自动连接: "RemoteControl: 自动连接已启用，正在连接..."
```

## 常见问题排查

### 问题1: 设置不生效
**症状**: 开关开启后，进入远程控制页面不自动连接
**排查**:
1. 检查日志中的 `autoConnectEnabled` 值是否为 true
2. 确认是否有保存的连接信息（检查 `lastIp` 和 `lastApiKey`）
3. 验证 SharedPreferences 文件是否一致

### 问题2: 设置值丢失
**症状**: 重启应用后设置恢复为 false
**排查**:
1. 确认使用的是持久化存储（apply() 已调用）
2. 检查是否有其他代码重置了该值

### 问题3: 上下文不一致
**症状**: 设置页面和远程控制页面读取的值不同
**解决**: 确保都使用 `App.app.pref` 而不是不同的上下文

## 代码变更总结

### 主要修改
1. **RemoteControlFragment.kt**: 统一使用 `App.app.pref` 读取设置
2. **SettingsPreferenceFragment.kt**: 添加设置变更日志
3. **新增工具类**: AutoConnectTester 用于调试和测试

### 关键代码片段
```kotlin
// RemoteControlFragment.kt 中读取设置
val settingsPrefs = App.app.pref
val autoConnectEnabled = settingsPrefs.getBoolean("remote.control.auto.connect", false)

// SettingsPreferenceFragment.kt 中设置监听器
findPreference<TwoStatePreference>("remote.control.auto.connect")!!.setOnPreferenceChangeListener { _, newValue ->
    Timber.d("Settings: 远程控制自动连接已设置为 $newValue")
    true
}
```

## 验证完成标准
- [ ] 设置开关状态能正确保存
- [ ] 设置值能在 RemoteControlFragment 正确读取
- [ ] 开启自动连接后能自动连接保存的设备
- [ ] 关闭自动连接后不进行自动连接
- [ ] 重启应用后设置值保持不变
- [ ] 日志中能正确显示设置值的变化

## 调试工具使用
```bash
# 使用测试工具类（需要集成到应用中）
# 在开发者选项中添加测试按钮调用 AutoConnectTester.logAutoConnectStatus()

# 手动调试命令
adb shell am start -n be.mygod.vpnhotspot/.SettingsActivity
adb logcat -c  # 清除日志
adb logcat | grep RemoteControl  # 查看实时日志
```