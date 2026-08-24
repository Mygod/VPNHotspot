# VPNHotspot 构建指南

## 系统要求
- **Java 17** (OpenJDK 17或更高版本)
- **Android SDK** (通过Android Studio或命令行工具)

## 各平台构建方法

### macOS
使用专用构建脚本：
```bash
./build-macos.sh
```

脚本会自动：
- 检测系统架构 (Apple Silicon/Intel)
- 配置正确的Java 17路径
- 执行完整构建

### Windows
使用Windows批处理脚本：
```cmd
build-windows.bat
```

脚本会：
- 检查Java安装
- 验证JAVA_HOME环境变量
- 提供配置指导

### Linux
手动设置环境：
```bash
export JAVA_HOME=/path/to/java17
./gradlew clean build
```

## Java 17安装

### macOS (Homebrew)
```bash
# Apple Silicon
brew install openjdk@17

# Intel
brew install openjdk@17
```

### Windows
1. 下载 [Adoptium OpenJDK 17](https://adoptium.net/)
2. 安装并配置JAVA_HOME环境变量

### Linux
```bash
# Ubuntu/Debian
sudo apt install openjdk-17-jdk

# CentOS/RHEL
sudo yum install java-17-openjdk-devel
```

## 故障排除

### 常见问题
1. **Java版本错误**：确保使用Java 17
2. **权限问题**：在macOS/Linux上使用`chmod +x`赋予脚本执行权限
3. **路径问题**：检查Java安装路径是否正确

### 验证环境
```bash
java -version
./gradlew --version
```