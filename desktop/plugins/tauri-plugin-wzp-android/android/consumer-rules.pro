# The plugin class is instantiated from Rust via JNI and its @Command methods
# are looked up by reflection; TransferService is referenced from the manifest.
-keep class com.wzp.plugin.** { *; }
