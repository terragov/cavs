// Detailed, bilingual help content shown in the "See more" modal.
// Field descriptions and expected-result text are shared/derived so every
// section gets specific, accurate detail without hand-writing an essay each.

import type { Lang } from "../api/types";

type Bi = { es: string; en: string };

export function pick(bi: Bi, lang: Lang): string {
  return bi[lang] ?? bi.en;
}

export const UI: Record<string, Bi> = {
  seeMore: { es: "Ver más", en: "See more" },
  detailTitle: { es: "Guía de la sección", en: "Section guide" },
  purpose: { es: "Para qué sirve", en: "What it's for" },
  howToUse: { es: "Cómo se usa", en: "How to use it" },
  expected: { es: "Resultado esperado", en: "Expected result" },
  fields: { es: "Campos", en: "Fields" },
  noForm: {
    es: "Esta sección no usa un formulario de creación; es una vista o herramienta.",
    en: "This section has no create form; it is a view or a tool.",
  },
  createStep: {
    es: "Pulsa «Crear» para abrir el formulario, complétalo y ejecútalo.",
    en: "Press “Create” to open the form, fill it in and run it.",
  },
  compareStep: {
    es: "Pulsa «Crear», arrastra (o elige) los dos elementos a comparar y pulsa comparar.",
    en: "Press “Create”, drag in (or pick) the two items to compare, then compare.",
  },
  wizardStep: {
    es: "Pulsa «Crear» y sigue el asistente paso a paso (puedes volver atrás).",
    en: "Press “Create” and follow the step-by-step wizard (you can go back).",
  },
  background: {
    es: "La operación corre en segundo plano y aparece en «Actividades» al terminar.",
    en: "The operation runs in the background and appears in “Activities” when done.",
  },
  historyNote: {
    es: "El historial de esta sección muestra todo lo que has hecho aquí; puedes ver el resultado, abrir la carpeta o eliminarlo.",
    en: "This section's history lists everything you have run here; open the result, open the folder, or delete it.",
  },
};

// Expected result per CAVS operation.
export const EXPECTED_BY_OP: Record<string, Bi> = {
  analyze: {
    es: "Un informe con el tamaño de la descarga completa vs. la actualización CAVS, el porcentaje de ahorro, un mapa de regiones cambiadas, diagnósticos en lenguaje claro y recomendaciones.",
    en: "A report with full-download vs. CAVS-update size, savings percentage, a changed-region map, plain-language diagnostics and recommendations.",
  },
  packDirectory: {
    es: "Un archivo de release .cavs guardado en la carpeta de esta operación, listo para servir o aplicar.",
    en: "A .cavs release file stored in this operation's folder, ready to serve or apply.",
  },
  createPlan: {
    es: "Un archivo .cavsplan más un resumen (operaciones, bytes reutilizados vs. inline, tamaño estimado de red).",
    en: "A .cavsplan file plus a summary (operations, reused vs. inline bytes, estimated network size).",
  },
  applyPlan: {
    es: "La build reconstruida escrita en la carpeta de esta operación, para comprobar que el plan reconstruye correctamente.",
    en: "The reconstructed build written to this operation's folder, so you can confirm the plan reconstructs correctly.",
  },
  verifyInstall: {
    es: "Un informe de verificación: archivos y bytes comprobados y cualquier diferencia (faltantes, extra, modificados).",
    en: "A verification report: files and bytes checked and any differences (missing, extra, modified).",
  },
  previewUpdate: {
    es: "Una tabla comparando rutas de entrega (descarga por ruta) con la ruta recomendada.",
    en: "A table comparing delivery routes (download per route) with the recommended route.",
  },
  benchmark: {
    es: "Tamaños y tiempos medidos para la actualización entre las dos entradas.",
    en: "Measured sizes and timings for the update between the two inputs.",
  },
  estimateSavings: {
    es: "El porcentaje de ahorro y la diferencia de costo mensual estimada de ancho de banda.",
    en: "The savings percentage and the estimated monthly bandwidth cost delta.",
  },
};

// Per-field explanation (what it is + why it matters).
export const FIELD_HELP: Record<string, Bi> = {
  oldPath: {
    es: "La versión anterior (build o archivo). Es la base: CAVS la compara con la nueva para calcular solo lo que cambió.",
    en: "The previous version (build or file). The baseline: CAVS compares it against the new one to compute only what changed.",
  },
  newPath: {
    es: "La versión nueva (build o archivo). Es lo que quieres entregar o analizar frente a la anterior.",
    en: "The new version (build or file). What you want to deliver or analyze against the previous one.",
  },
  inputDir: {
    es: "La carpeta que se va a empaquetar. Todo su contenido entra en el release (salvo lo ignorado).",
    en: "The folder to pack. All its contents go into the release (except ignored files).",
  },
  outputCavs: {
    es: "Nombre del archivo .cavs de salida. Se guarda dentro de la carpeta de la operación.",
    en: "Name of the output .cavs file. Stored inside the operation's folder.",
  },
  outputPlan: {
    es: "Nombre del archivo .cavsplan de salida (el plan de actualización). Se guarda en la carpeta de la operación.",
    en: "Name of the output .cavsplan file (the update plan). Stored in the operation's folder.",
  },
  outputPath: {
    es: "Carpeta de salida para la build reconstruida al aplicar.",
    en: "Output folder for the reconstructed build when applying.",
  },
  planPath: {
    es: "El archivo de plan/release a aplicar sobre la build anterior.",
    en: "The plan/release file to apply on top of the old build.",
  },
  target: {
    es: "El archivo o carpeta a verificar (por ejemplo un .cavs o una build).",
    en: "The file or folder to verify (for example a .cavs or a build).",
  },
  assetName: {
    es: "Identificador del asset (p. ej. game_content). El cliente lo usa para pedir la actualización correcta.",
    en: "Asset identifier (e.g. game_content). The client uses it to request the correct update.",
  },
  newVersion: {
    es: "La versión que estás generando (p. ej. 1.0.1). Etiqueta el release para el cliente.",
    en: "The version you are generating (e.g. 1.0.1). Labels the release for the client.",
  },
  version: {
    es: "La versión asociada al release.",
    en: "The version associated with the release.",
  },
  profile: {
    es: "Perfil de chunking (tamaño de bloque). Afecta cuánto se reutiliza entre versiones. FastCDC 16k minimiza los updates (−65% medido en builds reales); Auto mide candidatos sobre los bytes reales. Mantén el perfil de la versión anterior para conservar la reutilización.",
    en: "Chunking profile (block size). Affects how much is reused between versions. FastCDC 16k minimizes updates (−65% measured on real builds); Auto measures candidates on the real bytes. Keep the previous version's profile to preserve reuse.",
  },
  compression: {
    es: "Compresión del payload por chunk (zstd-<nivel> o ninguna). zstd-3 es el equilibrio por defecto; zstd-19 reduce ~9% más la descarga a cambio de más CPU al empaquetar (el cliente descomprime igual de rápido).",
    en: "Per-chunk payload compression (zstd-<level> or none). zstd-3 is the default balance; zstd-19 cuts downloads a further ~9% for more packing CPU (client decode speed is unchanged).",
  },
  signKeyPath: {
    es: "Llave de firma opcional. Si la indicas, el release se firma para verificar su integridad (no es DRM).",
    en: "Optional signing key. If provided, the release is signed so its integrity can be verified (not DRM).",
  },
  engineHint: {
    es: "El motor/perfil a asumir en el análisis. Ajusta cómo se interpretan los packs. Por defecto toma el del proyecto.",
    en: "The engine/profile to assume for analysis. Tunes how packs are interpreted. Defaults to the project's engine.",
  },
  pricePerGb: {
    es: "Cuánto pagas por GB de descarga. Base del cálculo de ahorro.",
    en: "How much you pay per GB of download. The basis for the savings calculation.",
  },
  monthlyDownloads: {
    es: "Descargas mensuales estimadas de la actualización.",
    en: "Estimated monthly downloads of the update.",
  },
  averageFullDownloadBytes: {
    es: "Tamaño promedio de una descarga completa (sin CAVS), en bytes.",
    en: "Average size of a full download (without CAVS), in bytes.",
  },
  averageCavsDownloadBytes: {
    es: "Tamaño promedio de la descarga con CAVS, en bytes.",
    en: "Average size of the CAVS download, in bytes.",
  },
};
