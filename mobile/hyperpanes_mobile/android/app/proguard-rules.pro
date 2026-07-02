# mobile_scanner → Google ML Kit barcode scanning resolves classes reflectively;
# R8 must not strip or rename them (release-build NPE on scanner open otherwise).
# Only consulted when isMinifyEnabled is turned back on.
-keep class com.google.mlkit.** { *; }
-keep class com.google.android.gms.vision.** { *; }
-keep class com.google.android.gms.internal.mlkit_vision_barcode.** { *; }
-keep class com.google.android.gms.internal.mlkit_vision_common.** { *; }
-dontwarn com.google.mlkit.**
