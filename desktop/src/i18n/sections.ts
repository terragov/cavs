// Bilingual section labels, taglines and per-section help (spec: every
// section shows help explaining how it works and what it is used for, in the
// user's selected language).

export interface SectionText {
  label: string;
  tagline: string;
  help: { summary: string; points: string[] };
  steps?: Record<string, string>;
}

type Bi = { en: SectionText; es: SectionText };

export const SECTION_TEXT: Record<string, Bi> = {
  home: {
    en: {
      label: "Dashboard",
      tagline: "Overview of this project.",
      help: {
        summary: "The starting point for the selected project: key numbers and quick actions.",
        points: [
          "See how many releases and analyses you have run.",
          "Jump straight into the most common workflows.",
          "Open the project's output folder.",
        ],
      },
    },
    es: {
      label: "Panel",
      tagline: "Resumen de este proyecto.",
      help: {
        summary: "El punto de partida del proyecto seleccionado: cifras clave y acciones rápidas.",
        points: [
          "Observa cuántos releases y análisis has ejecutado.",
          "Entra directo a los flujos de trabajo más comunes.",
          "Abre la carpeta de salida del proyecto.",
        ],
      },
    },
  },
  projects: {
    en: {
      label: "Projects",
      tagline: "Local CAVS Desktop projects.",
      help: {
        summary: "A project remembers your engine type, build paths, output folders and preferred workflow.",
        points: [
          "Create a project per product so paths and settings are remembered.",
          "Each project keeps its own releases and reports folders.",
          "Open a project to see its dashboard and recent work.",
        ],
      },
    },
    es: {
      label: "Proyectos",
      tagline: "Proyectos locales de CAVS Desktop.",
      help: {
        summary: "Un proyecto recuerda tu tipo de motor, rutas de compilación, carpetas de salida y flujo preferido.",
        points: [
          "Crea un proyecto por producto para recordar rutas y configuración.",
          "Cada proyecto guarda sus propias carpetas de releases e informes.",
          "Abre un proyecto para ver su panel y trabajo reciente.",
        ],
      },
    },
  },
  activities: {
    en: {
      label: "Activities",
      tagline: "Everything running and everything you have done.",
      help: {
        summary:
          "When you start an operation it runs in the background — you never have to wait. This section lists what is running now and everything already finished.",
        points: [
          "In-progress items keep working even if you navigate away.",
          "Open a finished item to review its result.",
          "Jump straight to the section an activity belongs to.",
        ],
      },
    },
    es: {
      label: "Actividades",
      tagline: "Todo lo que está en curso y todo lo que has hecho.",
      help: {
        summary:
          "Cuando inicias una operación se ejecuta en segundo plano — nunca tienes que esperar. Esta sección lista lo que está en curso y todo lo ya finalizado.",
        points: [
          "Lo que está en curso sigue trabajando aunque cambies de sección.",
          "Abre un elemento finalizado para revisar su resultado.",
          "Salta directo a la sección a la que pertenece cada actividad.",
        ],
      },
    },
  },
  "godot-runtime": {
    en: {
      label: "Runtime Update",
      tagline: "Download, verify, reconstruct and mount an updated pack at runtime.",
      help: {
        summary: "A guided wizard for the core Godot use case: turn two PCK files into a small runtime update.",
        points: [
          "Select a base PCK and an updated PCK, then name the asset and version.",
          "CAVS builds an update plan so the client downloads only what changed.",
          "Finish by starting a local server and copying the GDScript snippet.",
        ],
      },
      steps: {
        "steps.pcks": "Select PCKs",
        "steps.output": "Output & generate",
      },
    },
    es: {
      label: "Runtime Update",
      tagline: "Descarga, verifica, reconstruye y monta un pack actualizado en runtime.",
      help: {
        summary: "Un asistente guiado para el caso principal de Godot: convertir dos archivos PCK en una actualización pequeña.",
        points: [
          "Selecciona el PCK base y el PCK actualizado, luego nombra el asset y la versión.",
          "CAVS crea un plan de actualización para que el cliente descargue solo lo que cambió.",
          "Termina iniciando un servidor local y copiando el snippet de GDScript.",
        ],
      },
      steps: {
        "steps.pcks": "Selecciona los PCK",
        "steps.output": "Salida y generar",
      },
    },
  },
  "godot-plugin": {
    en: {
      label: "Plugin",
      tagline: "Install the runtime plugin for your engine and copy ready-made code.",
      help: {
        summary: "Onboarding for the CAVS engine plugins — Godot (stable), Unity and Unreal (beta).",
        points: [
          "See install instructions and a download link for your engine.",
          "Copy fetch/update, progress and error-handling snippets.",
          "Everything is copy-paste ready for your project.",
        ],
      },
    },
    es: {
      label: "Plugin",
      tagline: "Configura el plugin de runtime y copia código listo para usar.",
      help: {
        summary: "Convierte la app de escritorio en una herramienta de onboarding para el plugin de Godot de CAVS.",
        points: [
          "Consulta instrucciones de instalación y valores de configuración.",
          "Copia snippets de update_and_mount, señales de progreso y manejo de errores.",
          "Todo está listo para copiar y pegar en tu proyecto.",
        ],
      },
    },
  },
  "godot-pck-analyzer": {
    en: {
      label: "PCK Analyzer",
      tagline: "Specialized analysis for .pck files.",
      help: {
        summary: "Compares an old and a new .pck to show changed resources and estimate the runtime update size.",
        points: [
          "Drop old.pck and new.pck to compare them.",
          "See changed res:// resources and shifted regions.",
          "Get a recommendation on whether runtime resource-pack delivery fits.",
        ],
      },
    },
    es: {
      label: "Analizador PCK",
      tagline: "Análisis especializado de archivos .pck.",
      help: {
        summary: "Compara un .pck viejo y uno nuevo para mostrar recursos cambiados y estimar el tamaño de la actualización.",
        points: [
          "Arrastra old.pck y new.pck para compararlos.",
          "Observa qué recursos res:// cambiaron y las regiones desplazadas.",
          "Recibe una recomendación sobre si conviene la entrega por resource-pack en runtime.",
        ],
      },
    },
  },
  "build-analyzer": {
    en: {
      label: "Build Analyzer",
      tagline: "Understand why an update is large.",
      help: {
        summary: "Compares two build folders, explains why the update is large and suggests improvements.",
        points: [
          "Select an old build folder and a new build folder.",
          "See CAVS vs full-download sizes and a per-file cost breakdown.",
          "Read plain-language diagnostics and concrete recommendations.",
        ],
      },
    },
    es: {
      label: "Analizador de builds",
      tagline: "Entiende por qué una actualización es grande.",
      help: {
        summary: "Compara dos carpetas de build, explica por qué la actualización es grande y sugiere mejoras.",
        points: [
          "Selecciona una carpeta de build vieja y una nueva.",
          "Compara el tamaño CAVS vs. descarga completa y el costo por archivo.",
          "Lee diagnósticos en lenguaje claro y recomendaciones concretas.",
        ],
      },
    },
  },
  "pack-inspector": {
    en: {
      label: "Pack Inspector",
      tagline: "How patch-friendly is a pack file?",
      help: {
        summary: "Inspects large pack files to understand reuse, scattered changes and entropy.",
        points: [
          "Drop an old and new pack file to compare them.",
          "See reused vs changed regions and likely causes.",
          "Learn whether the change is localized (good) or scattered (bad).",
        ],
      },
    },
    es: {
      label: "Inspector de packs",
      tagline: "¿Qué tan apto para parches es un pack?",
      help: {
        summary: "Inspecciona archivos pack grandes para entender reutilización, cambios dispersos y entropía.",
        points: [
          "Arrastra un pack viejo y uno nuevo para compararlos.",
          "Observa regiones reutilizadas vs. cambiadas y sus causas probables.",
          "Descubre si el cambio es localizado (bueno) o disperso (malo).",
        ],
      },
    },
  },
  compare: {
    en: {
      label: "Compare Mode",
      tagline: "Compare two builds or strategies side by side.",
      help: {
        summary: "Runs a comparison between two inputs and shows the results together.",
        points: [
          "Drop a baseline and an optimized/alternative input.",
          "Compare download, changed regions, warnings and recommendations.",
          "Useful for before/after checks such as splitting a pack.",
        ],
      },
    },
    es: {
      label: "Modo comparación",
      tagline: "Compara dos builds o estrategias lado a lado.",
      help: {
        summary: "Ejecuta una comparación entre dos entradas y muestra los resultados juntos.",
        points: [
          "Arrastra una línea base y una entrada optimizada/alternativa.",
          "Compara descarga, regiones cambiadas, avisos y recomendaciones.",
          "Útil para verificar antes/después, como dividir un pack.",
        ],
      },
    },
  },
  savings: {
    en: {
      label: "Savings Estimate",
      tagline: "Estimate bandwidth savings.",
      help: {
        summary: "Estimates monthly bandwidth cost savings from switching full downloads to CAVS updates.",
        points: [
          "Enter your price per GB and monthly downloads.",
          "Enter the average full-download and CAVS-download sizes.",
          "Get the savings percentage and estimated monthly cost delta.",
        ],
      },
    },
    es: {
      label: "Estimación de ahorro",
      tagline: "Estima el ahorro de ancho de banda.",
      help: {
        summary: "Estima el ahorro mensual de costos de ancho de banda al pasar de descargas completas a actualizaciones CAVS.",
        points: [
          "Ingresa tu precio por GB y las descargas mensuales.",
          "Ingresa el tamaño promedio de descarga completa y de descarga CAVS.",
          "Obtén el porcentaje de ahorro y la diferencia de costo mensual estimada.",
        ],
      },
    },
  },
  generate: {
    en: {
      label: "Generate Update",
      tagline: "Create CAVS release artifacts.",
      help: {
        summary: "Packs a build folder into a .cavs release you can serve and apply.",
        points: [
          "Select the input build folder and an output file name.",
          "Choose a chunk profile and compression.",
          "The generated file is stored with this operation and can be verified.",
        ],
      },
    },
    es: {
      label: "Generar actualización",
      tagline: "Crea artefactos de release CAVS.",
      help: {
        summary: "Empaqueta una carpeta de build en un release .cavs que puedes servir y aplicar.",
        points: [
          "Selecciona la carpeta de build de entrada y un nombre de archivo de salida.",
          "Elige un perfil de chunk y la compresión.",
          "El archivo generado se guarda con esta operación y puede verificarse.",
        ],
      },
    },
  },
  "apply-verify": {
    en: {
      label: "Apply / Verify",
      tagline: "Test that updates reconstruct correctly.",
      help: {
        summary: "Applies a plan/release to an old build and produces the reconstructed output.",
        points: [
          "Select the old build folder and the plan/release file.",
          "The output is written to this operation's folder.",
          "Use it to build confidence before integrating CAVS into your product.",
        ],
      },
    },
    es: {
      label: "Aplicar / Verificar",
      tagline: "Comprueba que las actualizaciones se reconstruyen bien.",
      help: {
        summary: "Aplica un plan/release sobre una build vieja y produce la salida reconstruida.",
        points: [
          "Selecciona la carpeta de build vieja y el archivo de plan/release.",
          "La salida se escribe en la carpeta de esta operación.",
          "Úsalo para ganar confianza antes de integrar CAVS en tu producto.",
        ],
      },
    },
  },
  "file-inspector": {
    en: {
      label: "File Inspector",
      tagline: "Inspect and verify CAVS files.",
      help: {
        summary: "Verifies a target file or folder and reports what was checked.",
        points: [
          "Select a .cavs / .cavsplan / build target.",
          "See files checked, bytes checked and any mismatches.",
          "A quick integrity check for artifacts and outputs.",
        ],
      },
    },
    es: {
      label: "Inspector de archivos",
      tagline: "Inspecciona y verifica archivos CAVS.",
      help: {
        summary: "Verifica un archivo o carpeta objetivo e informa qué se comprobó.",
        points: [
          "Selecciona un objetivo .cavs / .cavsplan / build.",
          "Observa archivos comprobados, bytes comprobados y discrepancias.",
          "Una verificación rápida de integridad para artefactos y salidas.",
        ],
      },
    },
  },
  "publish-preview": {
    en: {
      label: "Publish Preview",
      tagline: "What would happen if you published this update?",
      help: {
        summary: "Compares delivery routes for an update and recommends the best one.",
        points: [
          "Select the previous build and the new build.",
          "See a route table with download size and notes.",
          "Get a recommended route with the reasoning.",
        ],
      },
    },
    es: {
      label: "Vista previa de publicación",
      tagline: "¿Qué pasaría si publicaras esta actualización?",
      help: {
        summary: "Compara rutas de entrega para una actualización y recomienda la mejor.",
        points: [
          "Selecciona la build previa y la nueva.",
          "Observa una tabla de rutas con tamaño de descarga y notas.",
          "Obtén una ruta recomendada con su justificación.",
        ],
      },
    },
  },
  "route-planner": {
    en: {
      label: "Route Planner",
      tagline: "Compare all update routes.",
      help: {
        summary: "Compares supported update routes so you can choose the best one by policy.",
        points: [
          "Select two builds to compare routes across them.",
          "See per-route download sizes side by side.",
          "Pick the smallest, fastest or most memory-friendly route.",
        ],
      },
    },
    es: {
      label: "Planificador de rutas",
      tagline: "Compara todas las rutas de actualización.",
      help: {
        summary: "Compara las rutas de actualización soportadas para elegir la mejor según tu política.",
        points: [
          "Selecciona dos builds para comparar rutas entre ellas.",
          "Observa los tamaños de descarga por ruta lado a lado.",
          "Elige la ruta más pequeña, más rápida o más liviana en memoria.",
        ],
      },
    },
  },
  benchmark: {
    en: {
      label: "Benchmark Lab",
      tagline: "Run controlled benchmarks.",
      help: {
        summary: "Benchmarks the update between two inputs, optionally measuring apply time.",
        points: [
          "Select an old and new input to benchmark.",
          "See measured sizes and timings.",
          "Export the result for sharing or comparison.",
        ],
      },
    },
    es: {
      label: "Laboratorio de benchmarks",
      tagline: "Ejecuta benchmarks controlados.",
      help: {
        summary: "Hace benchmark de la actualización entre dos entradas, midiendo opcionalmente el tiempo de aplicación.",
        points: [
          "Selecciona una entrada vieja y una nueva para el benchmark.",
          "Observa los tamaños y tiempos medidos.",
          "Exporta el resultado para compartir o comparar.",
        ],
      },
    },
  },
  workspace: {
    en: {
      label: "Workspace / Depots",
      tagline: "Model app/depot/branch/build workflows.",
      help: {
        summary: "Model realistic studio workflows: apps, depots, branches and builds. Managed today via the CAVS CLI.",
        points: [
          "Depots group platform/language content and track shared %.",
          "Branches (public/beta/nightly) let you promote and roll back builds.",
          "Use Create to pack a depot folder into a .cavs artifact.",
        ],
      },
    },
    es: {
      label: "Workspace / Depots",
      tagline: "Modela flujos de app/depot/branch/build.",
      help: {
        summary: "Modela flujos de estudio realistas: apps, depots, ramas y builds. Hoy se gestiona con el CLI de CAVS.",
        points: [
          "Los depots agrupan contenido por plataforma/idioma y miden el % compartido.",
          "Las ramas (public/beta/nightly) permiten promover y revertir builds.",
          "Usa Crear para empaquetar una carpeta de depot en un artefacto .cavs.",
        ],
      },
    },
  },
  "install-plan": {
    en: {
      label: "Install Plan",
      tagline: "What would a client download?",
      help: {
        summary: "Simulates a client download based on platform, language, ownership and installed version.",
        points: [
          "Choose platform, language and owned depots.",
          "See per-depot update size and shared content reused.",
          "Use Create to compare an installed build against a target build.",
        ],
      },
    },
    es: {
      label: "Plan de instalación",
      tagline: "¿Qué descargaría un cliente?",
      help: {
        summary: "Simula la descarga de un cliente según plataforma, idioma, propiedad y versión instalada.",
        points: [
          "Elige plataforma, idioma y depots que posee.",
          "Observa el tamaño de actualización por depot y el contenido compartido reutilizado.",
          "Usa Crear para comparar una build instalada contra una build objetivo.",
        ],
      },
    },
  },
  "shared-content": {
    en: {
      label: "Shared Content",
      tagline: "How much content is shared between depots?",
      help: {
        summary: "Shows how much content is shared across depots, platforms, demos, DLCs and language packs.",
        points: [
          "Compare depots pairwise or as a matrix.",
          "See dedup ratio and unique/shared breakdown.",
          "Use Create to compare two depots and see how much they share.",
        ],
      },
    },
    es: {
      label: "Contenido compartido",
      tagline: "¿Cuánto contenido se comparte entre depots?",
      help: {
        summary: "Muestra cuánto contenido se comparte entre depots, plataformas, demos, DLCs y packs de idioma.",
        points: [
          "Compara depots por pares o como matriz.",
          "Observa el ratio de deduplicación y el desglose único/compartido.",
          "Usa Crear para comparar dos depots y ver cuánto comparten.",
        ],
      },
    },
  },
  "build-history": {
    en: {
      label: "Build History",
      tagline: "Track generated releases over time.",
      help: {
        summary: "Tracks the releases you generated and lets you compare them over time.",
        points: [
          "Lists every generate operation with its size and status.",
          "See update size and savings trends across releases.",
          "Open a release's folder or view its full result.",
        ],
      },
    },
    es: {
      label: "Historial de builds",
      tagline: "Sigue los releases generados a lo largo del tiempo.",
      help: {
        summary: "Sigue los releases que generaste y te permite compararlos en el tiempo.",
        points: [
          "Lista cada operación de generación con su tamaño y estado.",
          "Observa tendencias de tamaño de actualización y ahorro entre releases.",
          "Abre la carpeta de un release o mira su resultado completo.",
        ],
      },
    },
  },
  "local-server": {
    en: {
      label: "Local Server",
      tagline: "Start and monitor a local test server.",
      help: {
        summary: "Serves a workspace/release folder over HTTP for plugin testing. Development only.",
        points: [
          "Pick a folder, set a port and start the server.",
          "Watch requests, bytes served and a live request log.",
          "Copy the URL or Godot config to test runtime updates.",
        ],
      },
    },
    es: {
      label: "Servidor local",
      tagline: "Inicia y monitorea un servidor de pruebas local.",
      help: {
        summary: "Sirve una carpeta de workspace/release por HTTP para probar el plugin. Solo desarrollo.",
        points: [
          "Elige una carpeta, define un puerto e inicia el servidor.",
          "Observa peticiones, bytes servidos y un registro en vivo.",
          "Copia la URL o la config de Godot para probar actualizaciones en runtime.",
        ],
      },
    },
  },
  "serverless-cdn": {
    en: {
      label: "Serverless CDN",
      tagline: "Publish a static export and update clients with no server.",
      help: {
        summary:
          "Export a release as an immutable static tree (packs + manifest + chunk-map) for S3/R2/Pages/nginx, then update clients straight from it with no cavs-server — via concurrent HTTP Range requests.",
        points: [
          "Build the export command from your store and output folder.",
          "Upload the folder to any static host that honours HTTP Range.",
          "Clients (CLI fetch-static, or the SDK/plugins) download only changed chunks in parallel.",
        ],
      },
    },
    es: {
      label: "CDN sin servidor",
      tagline: "Publica un export estático y actualiza clientes sin servidor.",
      help: {
        summary:
          "Exporta una release como un árbol estático inmutable (packs + manifest + chunk-map) para S3/R2/Pages/nginx, y actualiza a los clientes directamente desde ahí sin cavs-server — con peticiones HTTP Range concurrentes.",
        points: [
          "Construye el comando de export desde tu store y carpeta de salida.",
          "Sube la carpeta a cualquier host estático que soporte HTTP Range.",
          "Los clientes (CLI fetch-static, o el SDK/plugins) descargan en paralelo solo los chunks que cambiaron.",
        ],
      },
    },
  },
  "sdk-helper": {
    en: {
      label: "SDK / Pipeline",
      tagline: "Integrate CAVS into your pipeline.",
      help: {
        summary: "Shows installation and minimal examples for each CAVS SDK and generates pipeline snippets.",
        points: [
          "Pick a language to see install steps and a minimal example.",
          "Copy pack/generate/verify snippets.",
          "Copy CI templates (GitHub Actions, shell script).",
        ],
      },
    },
    es: {
      label: "SDK / Pipeline",
      tagline: "Integra CAVS en tu pipeline.",
      help: {
        summary: "Muestra instalación y ejemplos mínimos para cada SDK de CAVS y genera snippets de pipeline.",
        points: [
          "Elige un lenguaje para ver los pasos de instalación y un ejemplo mínimo.",
          "Copia snippets de pack/generate/verify.",
          "Copia plantillas de CI (GitHub Actions, script de shell).",
        ],
      },
    },
  },
  "cli-builder": {
    en: {
      label: "CLI Builder",
      tagline: "Build CLI commands visually.",
      help: {
        summary: "Lets you build a CAVS CLI command from a form and copy it.",
        points: [
          "Pick a command type and fill in the fields.",
          "See the live command update as you type.",
          "Copy the command or save it as a script.",
        ],
      },
    },
    es: {
      label: "Comandos CLI",
      tagline: "Construye comandos CLI visualmente.",
      help: {
        summary: "Te permite construir un comando del CLI de CAVS desde un formulario y copiarlo.",
        points: [
          "Elige un tipo de comando y completa los campos.",
          "Observa cómo el comando se actualiza en vivo mientras escribes.",
          "Copia el comando o guárdalo como script.",
        ],
      },
    },
  },
  reports: {
    en: {
      label: "Reports",
      tagline: "All generated reports in one place.",
      help: {
        summary: "Collects every analysis, preview and benchmark you have run across sections.",
        points: [
          "Search and filter reports by section and date.",
          "Open a report to see its full result.",
          "Open its folder to access the exported JSON.",
        ],
      },
    },
    es: {
      label: "Informes",
      tagline: "Todos los informes generados en un solo lugar.",
      help: {
        summary: "Reúne cada análisis, vista previa y benchmark que ejecutaste en todas las secciones.",
        points: [
          "Busca y filtra informes por sección y fecha.",
          "Abre un informe para ver su resultado completo.",
          "Abre su carpeta para acceder al JSON exportado.",
        ],
      },
    },
  },
  recommendations: {
    en: {
      label: "Recommendations",
      tagline: "All recommendations, collected.",
      help: {
        summary: "Collects recommendations from every analysis so you can act on them in one place.",
        points: [
          "See critical issues, warnings and optimization opportunities.",
          "Each item links back to the analysis that produced it.",
          "Copy the issue text to share with your team.",
        ],
      },
    },
    es: {
      label: "Recomendaciones",
      tagline: "Todas las recomendaciones, reunidas.",
      help: {
        summary: "Reúne las recomendaciones de cada análisis para que actúes sobre ellas en un solo lugar.",
        points: [
          "Observa problemas críticos, avisos y oportunidades de optimización.",
          "Cada elemento enlaza al análisis que lo produjo.",
          "Copia el texto del problema para compartirlo con tu equipo.",
        ],
      },
    },
  },
  "engine-profiles": {
    en: {
      label: "Engine Profiles",
      tagline: "Manage engine-specific assumptions.",
      help: {
        summary: "Engine profiles (Godot, Unity, Unreal, Generic, Custom) set file patterns and pack extensions used by analysis.",
        points: [
          "The Godot profile is the most polished.",
          "Profiles are applied when you pick an engine in analysis sections.",
          "Advanced/custom rules are configured via the CLI today.",
        ],
      },
    },
    es: {
      label: "Perfiles de motor",
      tagline: "Gestiona supuestos específicos del motor.",
      help: {
        summary: "Los perfiles de motor (Godot, Unity, Unreal, Genérico, Personalizado) definen patrones de archivo y extensiones de pack usadas por el análisis.",
        points: [
          "El perfil de Godot es el más pulido.",
          "Los perfiles se aplican al elegir un motor en las secciones de análisis.",
          "Las reglas avanzadas/personalizadas se configuran hoy con el CLI.",
        ],
      },
    },
  },
  "ignore-rules": {
    en: {
      label: "Ignore Rules Editor",
      tagline: "Files that should not be packed/analyzed.",
      help: {
        summary: "Configure a .cavsignore so temporary and build-noise files are excluded.",
        points: [
          "Add glob patterns like *.pdb, logs/ and *.tmp.",
          "Ignored files are skipped when packing and analyzing.",
          "Edit .cavsignore in your project; test patterns via the CLI.",
        ],
      },
    },
    es: {
      label: "Editor de reglas de ignorado",
      tagline: "Archivos que no deben empaquetarse/analizarse.",
      help: {
        summary: "Configura un .cavsignore para excluir archivos temporales y ruido de build.",
        points: [
          "Agrega patrones glob como *.pdb, logs/ y *.tmp.",
          "Los archivos ignorados se omiten al empaquetar y analizar.",
          "Edita .cavsignore en tu proyecto; prueba patrones con el CLI.",
        ],
      },
    },
  },
  security: {
    en: {
      label: "Security",
      tagline: "Signing, verification and local encryption.",
      help: {
        summary: "Manage signing keys, sign and verify releases. This is for integrity and controlled testing — not DRM.",
        points: [
          "Generate a signing key and sign releases.",
          "Verify a release against a public key.",
          "Not DRM, license enforcement or anti-tamper.",
        ],
      },
    },
    es: {
      label: "Seguridad",
      tagline: "Firma, verificación y cifrado local.",
      help: {
        summary: "Gestiona llaves de firma, firma y verifica releases. Esto es para integridad y pruebas controladas — no DRM.",
        points: [
          "Genera una llave de firma y firma releases.",
          "Verifica un release contra una llave pública.",
          "No es DRM, control de licencias ni anti-tamper.",
        ],
      },
    },
  },
  cache: {
    en: {
      label: "Cache",
      tagline: "Inspect and manage CAVS caches.",
      help: {
        summary: "Inspect the local chunk cache: size, chunk count and unused/corrupt chunks.",
        points: [
          "See cache size and verify integrity.",
          "Clean unused chunks to reclaim space.",
          "Cleaning may make future updates download more data.",
        ],
      },
    },
    es: {
      label: "Caché",
      tagline: "Inspecciona y gestiona las cachés de CAVS.",
      help: {
        summary: "Inspecciona la caché local de chunks: tamaño, cantidad de chunks y chunks sin usar/corruptos.",
        points: [
          "Observa el tamaño de la caché y verifica su integridad.",
          "Limpia chunks sin usar para recuperar espacio.",
          "Limpiar puede hacer que futuras actualizaciones descarguen más datos.",
        ],
      },
    },
  },
  export: {
    en: {
      label: "Export",
      tagline: "Export results for sharing.",
      help: {
        summary: "Every operation already stores its result as JSON on disk; export opens that folder.",
        points: [
          "Results are saved as result.json next to their artifacts.",
          "Open any operation's folder to grab the files.",
          "Markdown/CSV/PNG export is on the roadmap.",
        ],
      },
    },
    es: {
      label: "Exportar",
      tagline: "Exporta resultados para compartir.",
      help: {
        summary: "Cada operación ya guarda su resultado como JSON en disco; exportar abre esa carpeta.",
        points: [
          "Los resultados se guardan como result.json junto a sus artefactos.",
          "Abre la carpeta de cualquier operación para tomar los archivos.",
          "La exportación a Markdown/CSV/PNG está en la hoja de ruta.",
        ],
      },
    },
  },
  docs: {
    en: {
      label: "Documentation",
      tagline: "Read the docs inside the app.",
      help: {
        summary: "Quick-start guides for the most common CAVS workflows.",
        points: [
          "Godot quick start and runtime PCK updates.",
          "CLI basics and SDK integration.",
          "Open the full docs website for more.",
        ],
      },
    },
    es: {
      label: "Documentación",
      tagline: "Lee la documentación dentro de la app.",
      help: {
        summary: "Guías de inicio rápido para los flujos de CAVS más comunes.",
        points: [
          "Inicio rápido de Godot y actualizaciones de PCK en runtime.",
          "Fundamentos del CLI e integración con SDKs.",
          "Abre el sitio de documentación completo para más.",
        ],
      },
    },
  },
  logs: {
    en: {
      label: "Logs",
      tagline: "Make failures understandable.",
      help: {
        summary: "Shows recent failed operations with plain-language error details.",
        points: [
          "Lists failures across every section.",
          "Each shows a stable error code and suggested actions.",
          "Copy diagnostics to share when filing an issue.",
        ],
      },
    },
    es: {
      label: "Registros",
      tagline: "Haz comprensibles los fallos.",
      help: {
        summary: "Muestra operaciones fallidas recientes con detalles de error en lenguaje claro.",
        points: [
          "Lista fallos en todas las secciones.",
          "Cada uno muestra un código de error estable y acciones sugeridas.",
          "Copia el diagnóstico para compartir al reportar un problema.",
        ],
      },
    },
  },
  feedback: {
    en: {
      label: "Feedback",
      tagline: "File a good issue easily.",
      help: {
        summary: "Generates a well-structured Markdown issue body you can paste into GitHub.",
        points: [
          "Describe what you tried, what happened and what you expected.",
          "The app formats it as Markdown.",
          "Open the CAVS issue tracker with one click.",
        ],
      },
    },
    es: {
      label: "Feedback",
      tagline: "Reporta un buen problema fácilmente.",
      help: {
        summary: "Genera un cuerpo de issue en Markdown bien estructurado para pegar en GitHub.",
        points: [
          "Describe qué intentaste, qué pasó y qué esperabas.",
          "La app lo formatea como Markdown.",
          "Abre el tracker de issues de CAVS con un clic.",
        ],
      },
    },
  },
  settings: {
    en: {
      label: "Settings",
      tagline: "Configure global desktop behavior.",
      help: {
        summary: "Global settings: language, theme, default folders, external tools and the local server.",
        points: [
          "Switch language (English/Spanish) and theme (dark/light).",
          "Detect external tools like zstd, xdelta3 and Godot.",
          "CAVS Desktop runs locally with no telemetry by default.",
        ],
      },
    },
    es: {
      label: "Configuración",
      tagline: "Configura el comportamiento global de la app.",
      help: {
        summary: "Configuración global: idioma, tema, carpetas por defecto, herramientas externas y el servidor local.",
        points: [
          "Cambia idioma (inglés/español) y tema (oscuro/claro).",
          "Detecta herramientas externas como zstd, xdelta3 y Godot.",
          "CAVS Desktop corre localmente y sin telemetría por defecto.",
        ],
      },
    },
  },
};
