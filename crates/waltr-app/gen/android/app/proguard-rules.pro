# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

-keep class dev.waltr.app.WaltrForegroundService { *; }
-keep class dev.waltr.app.SecureTokenStore { *; }
-keep class dev.waltr.app.SecureTokenStore$Companion { *; }
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }
# Rust calls these Activity methods by name through JNI.
-keepclassmembers class dev.waltr.app.MainActivity {
    public void requestLocalServer();
    public void selectRemoteServer();
    public void requestRecordAudioPermission(long);
}
-keepclasseswithmembernames class dev.waltr.app.MainActivity {
    native <methods>;
}
-keepclasseswithmembernames class dev.waltr.app.WaltrForegroundService {
    native <methods>;
}
