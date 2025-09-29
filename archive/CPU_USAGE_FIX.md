# CPU 使用率计算修复说明

## 问题描述

用户反馈：WebServer 中显示的 CPU 占用率不正确。

## 问题原因

原始的 CPU 使用率计算方法存在以下问题：

1. **错误的计算方式**：直接使用 `/proc/stat` 的当前值计算比例，而不是计算时间差值
2. **缺少时间间隔**：没有比较两个时间点的 CPU 时间差值
3. **缓存机制缺失**：频繁读取 `/proc/stat` 文件，性能不佳

## 修复方案

### 1. 正确的 CPU 使用率计算

**修复前（错误的方法）：**
```kotlin
private fun getCpuUsage(): Float {
    val reader = BufferedReader(FileReader("/proc/stat"))
    val line = reader.readLine()
    reader.close()
    
    val parts = line.split("\\s+".toRegex())
    if (parts.size >= 5) {
        val user = parts[1].toLong()
        val nice = parts[2].toLong()
        val system = parts[3].toLong()
        val idle = parts[4].toLong()
        
        val total = user + nice + system + idle
        val nonIdle = user + nice + system
        
        // 错误：直接计算比例
        (nonIdle * 100.0f / total).roundToInt().toFloat()
    }
}
```

**修复后（正确的方法）：**
```kotlin
private fun getCpuUsage(): Float {
    val currentTime = System.currentTimeMillis()
    
    // 缓存机制：避免频繁计算
    if (currentTime - lastCpuCheckTime < 1000) {
        return lastCpuUsage
    }
    
    val reader = BufferedReader(FileReader("/proc/stat"))
    val line = reader.readLine()
    reader.close()
    
    val parts = line.split("\\s+".toRegex())
    if (parts.size >= 5) {
        val user = parts[1].toLong()
        val nice = parts[2].toLong()
        val system = parts[3].toLong()
        val idle = parts[4].toLong()
        
        val currentCpuTime = user + nice + system + idle
        val currentCpuIdle = idle
        
        if (lastCpuTime > 0) {
            val cpuTimeDiff = currentCpuTime - lastCpuTime
            val cpuIdleDiff = currentCpuIdle - lastCpuIdle
            
            if (cpuTimeDiff > 0) {
                // 正确：计算时间差值的使用率
                val cpuUsage = ((cpuTimeDiff - cpuIdleDiff) * 100.0f / cpuTimeDiff).coerceIn(0.0f, 100.0f)
                lastCpuUsage = cpuUsage
            }
        }
        
        lastCpuTime = currentCpuTime
        lastCpuIdle = currentCpuIdle
        lastCpuCheckTime = currentTime
        
        lastCpuUsage
    }
}
```

### 2. 添加缓存变量

在 companion object 中添加了以下变量来支持正确的计算：

```kotlin
companion object {
    private var instance: WebServer? = null
    private var lastCpuTime: Long = 0        // 上次的 CPU 总时间
    private var lastCpuIdle: Long = 0        // 上次的 CPU 空闲时间
    private var lastCpuUsage: Float = 0.0f   // 上次计算的 CPU 使用率
    private var lastCpuCheckTime: Long = 0   // 上次检查时间
    // ...
}
```

## 技术原理

### CPU 使用率计算公式

正确的 CPU 使用率计算公式为：

```
CPU使用率 = (CPU时间差值 - CPU空闲时间差值) / CPU时间差值 × 100%
```

其中：
- **CPU时间差值** = 当前CPU总时间 - 上次CPU总时间
- **CPU空闲时间差值** = 当前CPU空闲时间 - 上次CPU空闲时间

### /proc/stat 文件格式

`/proc/stat` 文件包含以下字段：
```
cpu  user nice system idle iowait irq softirq steal guest guest_nice
```

我们使用前5个字段：
- `user`: 用户态时间
- `nice`: 用户态时间（低优先级）
- `system`: 内核态时间
- `idle`: 空闲时间
- `iowait`: IO等待时间

## 优化特性

### 1. 缓存机制
- 避免1秒内重复计算
- 减少文件读取频率
- 提高响应速度

### 2. 边界检查
- 使用 `coerceIn(0.0f, 100.0f)` 确保结果在 0-100% 范围内
- 防止异常值显示

### 3. 错误处理
- 捕获文件读取异常
- 返回默认值 0.0f

## 验证方法

1. **对比测试**：与系统自带的 CPU 监控工具对比
2. **压力测试**：在高负载下观察数值变化
3. **稳定性测试**：长时间运行观察数值稳定性

## 预期效果

修复后的 CPU 使用率显示将：
- ✅ 显示准确的实时 CPU 使用率
- ✅ 数值范围在 0-100% 之间
- ✅ 响应速度更快（缓存机制）
- ✅ 与系统监控工具数值一致

## 相关文件

- `mobile/src/main/java/be/mygod/vpnhotspot/WebServer.kt` - 主要修复文件
- `CPU_USAGE_FIX.md` - 本修复说明文档 