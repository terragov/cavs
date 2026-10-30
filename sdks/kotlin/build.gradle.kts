plugins {
    kotlin("jvm") version "2.0.21"
    kotlin("plugin.serialization") version "2.0.21"
    `java-library`
    `maven-publish`
}

group = "io.github.orelvis15"
version = "1.8.0"

kotlin {
    // The FFM native bridge (JEP 454) is finalized in Java 22.
    jvmToolchain(22)
}

java {
    withSourcesJar()
}

repositories {
    mavenCentral()
}

dependencies {
    api("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")

    testImplementation(kotlin("test"))
    testImplementation(platform("org.junit:junit-bom:5.10.3"))
    testImplementation("org.junit.jupiter:junit-jupiter")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher")
}

tasks.test {
    useJUnitPlatform()
    // FFM downcalls/upcalls require native access to be enabled.
    jvmArgs("--enable-native-access=ALL-UNNAMED")
    // Point the loader at a locally built native library via CAVS_SDK_LIBRARY.
    System.getenv("CAVS_SDK_LIBRARY")?.let { systemProperty("cavs.sdk.library", it) }
}

// ---- Runnable examples (see examples/README.md) ----
//
// The examples live in their own source set so they never end up in the
// published library jar. Run them with `gradle runQuickstart` / `runPreview`.
sourceSets {
    create("examples") {
        compileClasspath += sourceSets.main.get().output
        runtimeClasspath += sourceSets.main.get().output
        kotlin.srcDir("examples")
    }
}

// The examples need the same runtime dependencies as the library itself
// (kotlinx.serialization), so inherit main's dependency configurations.
configurations["examplesImplementation"].extendsFrom(configurations["implementation"], configurations["api"])
configurations["examplesRuntimeOnly"].extendsFrom(configurations["runtimeOnly"])

// A small helper that wires up native access and the CAVS_SDK_LIBRARY
// override the same way the test task does. The example classes are compiled
// for Java 22, so run them on the Java 22 toolchain launcher rather than
// whatever JVM Gradle itself happens to run on.
val javaToolchainService = extensions.getByType<JavaToolchainService>()
val exampleLauncher = javaToolchainService.launcherFor {
    languageVersion.set(JavaLanguageVersion.of(22))
}

fun registerExample(taskName: String, entryPoint: String) {
    tasks.register<JavaExec>(taskName) {
        group = "examples"
        javaLauncher.set(exampleLauncher)
        classpath = sourceSets["examples"].runtimeClasspath
        mainClass.set(entryPoint)
        jvmArgs("--enable-native-access=ALL-UNNAMED")
        System.getenv("CAVS_SDK_LIBRARY")?.let { systemProperty("cavs.sdk.library", it) }
    }
}

registerExample("runQuickstart", "com.cavs.examples.QuickstartKt")
registerExample("runPreview", "com.cavs.examples.PreviewKt")

publishing {
    publications {
        create<MavenPublication>("maven") {
            from(components["java"])
            artifactId = "cavs-sdk"
            pom {
                name = "CAVS SDK"
                description = "Kotlin/JVM SDK for CAVS, over the native Rust core via the C ABI"
                url = "https://github.com/orelvis15/cavs-oss"
                licenses {
                    license {
                        name = "Apache-2.0"
                        url = "https://www.apache.org/licenses/LICENSE-2.0"
                    }
                }
            }
        }
    }
    repositories {
        // Target repository is supplied by the release workflow (Maven
        // Central portal, GitHub Packages, or any Maven repo). When unset,
        // only `publishToMavenLocal` is usable.
        val repoUrl = providers.gradleProperty("mavenRepoUrl")
            .orElse(providers.environmentVariable("MAVEN_REPO_URL"))
        if (repoUrl.isPresent && repoUrl.get().isNotBlank()) {
            maven {
                url = uri(repoUrl.get())
                credentials {
                    username = providers.gradleProperty("mavenRepoUser")
                        .orElse(providers.environmentVariable("MAVEN_REPO_USER")).orNull
                    password = providers.gradleProperty("mavenRepoPassword")
                        .orElse(providers.environmentVariable("MAVEN_REPO_PASSWORD")).orNull
                }
            }
        }
    }
}
