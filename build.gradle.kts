plugins {
    kotlin("jvm") version "1.9.22"
    kotlin("plugin.serialization") version "1.9.22"
    application
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

    // YAML
    implementation("com.charleskorn.kaml:kaml:0.57.0")

    // Playwright (for browser-based authentication)
    implementation("com.microsoft.playwright:playwright:1.40.0")

    // Testing
    testImplementation(kotlin("test"))
}

application {
    mainClass.set("pro.dev.tt.MainKt")
}

tasks.test {
    useJUnitPlatform()
}

kotlin {
    jvmToolchain(17)
}
