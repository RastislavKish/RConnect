plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "com.rastislavkish.rconnect"
    compileSdk {
        version = release(36) {
            minorApiLevel = 1
        }
    }

    defaultConfig {
        applicationId = "com.rastislavkish.rconnect"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "1.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        // The Rust core is built for these ABIs only (see mobile/scripts/build-rust.sh).
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }
    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation("${libs.jna.get()}@aar")
    implementation(libs.kotlinx.coroutines.android)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    testImplementation(libs.junit)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.compose.ui.test.junit4)
    androidTestImplementation(libs.androidx.espresso.core)
    androidTestImplementation(libs.androidx.junit)
    debugImplementation(libs.androidx.compose.ui.test.manifest)
    debugImplementation(libs.androidx.compose.ui.tooling)
}

// The Rust core (mobile/core, mobile/ffi), its Kotlin bindings and eSpeak NG's voice data
// are built by the scripts in mobile/scripts before every build; cargo and CMake skip
// work that is up to date.
val scripts = rootDir.resolve("../scripts")
val rustSources = listOf("../../protocol", "../core", "../ffi").map { rootDir.resolve(it) }
val rustJniLibs = layout.buildDirectory.dir("rust/jniLibs")
val rustBindings = layout.buildDirectory.dir("rust/kotlin")
val espeakAssets = layout.buildDirectory.dir("rust/assets")

val buildRust by tasks.registering(Exec::class) {
    description = "Builds the Rust core for Android"
    inputs.files(rustSources.map { fileTree(it) { exclude("target") } })
    inputs.file(scripts.resolve("build-rust.sh"))
    outputs.dir(rustJniLibs)
    commandLine(scripts.resolve("build-rust.sh"), rustJniLibs.get().asFile)
}

val generateBindings by tasks.registering(Exec::class) {
    description = "Generates the Kotlin bindings of the Rust core"
    inputs.files(rustSources.map { fileTree(it) { exclude("target") } })
    inputs.file(scripts.resolve("generate-bindings.sh"))
    outputs.dir(rustBindings)
    commandLine(scripts.resolve("generate-bindings.sh"), rustBindings.get().asFile)
}

val buildEspeakData by tasks.registering(Exec::class) {
    description = "Compiles eSpeak NG's voice data"
    inputs.dir(rootDir.resolve("../third_party/espeak-ng/dictsource"))
    inputs.dir(rootDir.resolve("../third_party/espeak-ng/phsource"))
    inputs.file(scripts.resolve("build-espeak-data.sh"))
    outputs.dir(espeakAssets)
    commandLine(scripts.resolve("build-espeak-data.sh"), espeakAssets.get().asFile)
}

tasks.named("preBuild") { dependsOn(buildRust, generateBindings, buildEspeakData) }

androidComponents {
    onVariants { variant ->
        listOf(rustJniLibs, rustBindings, espeakAssets).forEach { it.get().asFile.mkdirs() }
        variant.sources.jniLibs?.addStaticSourceDirectory(rustJniLibs.get().asFile.path)
        variant.sources.kotlin?.addStaticSourceDirectory(rustBindings.get().asFile.path)
        variant.sources.assets?.addStaticSourceDirectory(espeakAssets.get().asFile.path)
    }
}
