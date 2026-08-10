import org.gradle.api.DefaultTask
import org.gradle.api.provider.ListProperty
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.TaskAction

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

val protocolFixturesDir = layout.buildDirectory.dir("generated/protocolFixtures")
val syncProtocolFixtures by tasks.registering(Sync::class) {
    from(rootProject.layout.projectDirectory.dir("../contracts/android/v2"))
    into(protocolFixturesDir)
}
val releaseKeystore = providers.environmentVariable("DAIMONOS_ANDROID_KEYSTORE").orNull
val releaseStorePassword = providers.environmentVariable("DAIMONOS_ANDROID_STORE_PASSWORD").orNull
val releaseKeyAlias = providers.environmentVariable("DAIMONOS_ANDROID_KEY_ALIAS").orNull
val releaseKeyPassword = providers.environmentVariable("DAIMONOS_ANDROID_KEY_PASSWORD").orNull
val releaseSigningValues = mapOf(
    "DAIMONOS_ANDROID_KEYSTORE" to releaseKeystore,
    "DAIMONOS_ANDROID_STORE_PASSWORD" to releaseStorePassword,
    "DAIMONOS_ANDROID_KEY_ALIAS" to releaseKeyAlias,
    "DAIMONOS_ANDROID_KEY_PASSWORD" to releaseKeyPassword,
)
val hasReleaseSigning = releaseSigningValues.values.all { it != null }

abstract class VerifyReleaseSigning : DefaultTask() {
    @get:Input
    abstract val variableNames: ListProperty<String>

    @TaskAction
    fun verify() {
        val missing = variableNames.get().filter { System.getenv(it).isNullOrEmpty() }
        if (missing.isNotEmpty()) {
            throw GradleException("Release signing variables are missing: ${missing.joinToString()}")
        }
    }
}

val verifyReleaseSigning by tasks.registering(VerifyReleaseSigning::class) {
    variableNames.set(releaseSigningValues.keys.toList())
}

android {
    namespace = "dev.daimonos.remote"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.daimonos.remote"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = file(checkNotNull(releaseKeystore))
                storePassword = checkNotNull(releaseStorePassword)
                keyAlias = checkNotNull(releaseKeyAlias)
                keyPassword = checkNotNull(releaseKeyPassword)
            }
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.findByName("release")
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    testOptions {
        unitTests.isIncludeAndroidResources = true
    }

    sourceSets {
        getByName("test").resources.directories.add(
            protocolFixturesDir.get().asFile.absolutePath,
        )
    }
}

dependencies {
    val composeBom = platform(libs.androidx.compose.bom)
    implementation(composeBom)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.kotlinx.serialization.json)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.okhttp)
    implementation(libs.bouncycastle)

    debugImplementation(libs.androidx.compose.ui.tooling)

    testImplementation(libs.junit)
    testImplementation(libs.mockwebserver)
}

tasks.configureEach {
    if (name.startsWith("process") && name.endsWith("UnitTestJavaRes")) {
        dependsOn(syncProtocolFixtures)
    }
    if (name == "packageRelease" || name == "bundleRelease") {
        dependsOn(verifyReleaseSigning)
    }
}
