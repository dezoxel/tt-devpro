import org.jetbrains.kotlin.gradle.tasks.KotlinCompile

plugins {
    kotlin("jvm") version "1.9.22"
    kotlin("plugin.serialization") version "1.9.22"
    application
    id("org.graalvm.buildtools.native") version "0.10.3"
}

group = "pro.dev"
version = "0.1.0"

repositories {
    mavenCentral()
}

dependencies {
    // CLI
    implementation("com.github.ajalt.clikt:clikt:4.2.2")

    // HTTP Client
    implementation("io.ktor:ktor-client-core:2.3.7")
    implementation("io.ktor:ktor-client-cio:2.3.7")
    implementation("io.ktor:ktor-client-content-negotiation:2.3.7")
    implementation("io.ktor:ktor-serialization-kotlinx-json:2.3.7")

    // JSON
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.6.2")

    // Logging (suppress SLF4J warnings from Ktor)
    implementation("org.slf4j:slf4j-nop:2.0.9")

    // YAML
    implementation("com.charleskorn.kaml:kaml:0.57.0")

    // Testing
    testImplementation(kotlin("test"))
}

application {
    mainClass.set("pro.dev.tt.MainKt")
}

tasks.test {
    useJUnitPlatform()
}

// No jvmToolchain: compile with whatever JDK runs Gradle (install.sh points
// JAVA_HOME at the GraalVM install so compile/installDist/nativeCompile all
// share one JDK). Target 17 bytecode — GraalVM-21 compiles it fine, and the
// JVM-fallback launcher then runs on any host JDK >= 17. The task-level DSL is
// the stable, non-opt-in form for Kotlin 1.9.x.
java {
    sourceCompatibility = JavaVersion.VERSION_17
    targetCompatibility = JavaVersion.VERSION_17
}

tasks.withType<KotlinCompile> {
    kotlinOptions.jvmTarget = "17"
}

graalvmNative {
    // Use the Gradle-running JDK (our JAVA_HOME GraalVM) directly rather than
    // trying to detect a GraalVM toolchain by vendor — Gradle mis-reads the
    // Oracle GraalVM cask's vendor, so detection is unreliable (gradle#25521).
    toolchainDetection.set(false)
    binaries.named("main") {
        imageName.set("tt-devpro")
        mainClass.set("pro.dev.tt.MainKt")
    }
}
