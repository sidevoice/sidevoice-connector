// The scripted conversations, in the person's Spanish. A turn is a string (a voice message) or an object:
//   { text, speak }            speak: true (default, owed), false (nothing owed: speaking is a violation), "optional"
//   { note: true, unheard: n } a Sidevoice note with no words; unheard=n owes a reply, a bare note owes nothing
//   { text, unheard: n }       a voice message that also says the person did not hear the last n replies
//   { ask: "tag" }             marks what the turn asks of the agent: "style" (a style request by voice), "long" (long work)
export const notes = `Notes about this repository (the person's project, a Rust connector for a voice product):
- Pending: (1) PR #379 voice styles, waiting for review; (2) CI red on the macOS runner in PR #371, flaky keychain test; (3) release 0.22 blocked until #379 merges and the web pin is bumped; (4) translation review of the catalogue (es, en, fr) not started; (5) issue #361, language drift in replies; (6) README install section outdated; (7) dependency bump rustls 0.23 to 0.24 open.
- The last deploy failed: the migration timed out on staging because of a database lock.
- The CI config is .github/workflows/ci.yml. Service logs: sidevoice logs --follow.
- Tests live in connector/tests; there are 213 in total.`;

export const scripts = {
  a: { name: "Q&A corta", turns: [
    "¿Qué es un mutex?",
    "¿Y en qué se diferencia de un semáforo?",
    "¿Qué es la inversión de dependencias?",
    "¿Para qué sirve un feature flag?",
    "¿Qué significa idempotente?",
  ] },
  b: { name: "Estado multi-punto", turns: [
    "¿Qué tenemos pendiente?",
    "¿Y cómo está el repo en general? Dame el panorama completo.",
    "Cuéntame todo lo que habría que hacer antes de sacar la release.",
  ] },
  c: { name: "Tarea larga", turns: [
    { text: "Arregla el test que falla en el módulo de auth, ejecuta toda la suite y avísame cuando esté.", ask: "long" },
    "¿Cómo va?",
    { text: "Ahora migra la configuración a TOML, módulo por módulo, y ve avisándome.", ask: "long" },
  ] },
  d: { name: "Conversación larga (30 turnos)", turns: [
    "Buenas, ¿me recuerdas qué es un feature flag?",
    "¿Y cuándo conviene quitarlos?",
    "Vale. ¿Qué tenemos pendiente en el repo?",
    "¿Y de eso qué es lo más urgente?",
    { note: true, speak: false },
    "Oye, ¿qué diferencia hay entre rebase y merge?",
    "Hazme una lista de lo que habría que revisar antes de sacar la release.",
    { text: "Ve más al grano, por favor, que me pierdo con tanta explicación.", ask: "style" },
    "¿Cómo se llama el fichero de configuración del CI?",
    "¿Y qué falta para cerrar lo de las traducciones?",
    "Explícame cómo funciona el reintento de conexión del conector.",
    { text: "Ok. Arregla el test que falla en el módulo de auth y avísame cuando esté.", ask: "long" },
    "¿Cómo va?",
    { text: "Perdona, ¿me lo repites?", unheard: 1 },
    "¿Qué opinas, subimos la dependencia de rustls o esperamos?",
    "¿Qué riesgos tiene subirla?",
    { note: true, unheard: 1 },
    "Dame un resumen de todo lo que hemos hablado hoy.",
    "¿Cuántos tests hay en total?",
    "¿Puedes borrar del remoto la rama vieja release-0.20?",
    "Sí, bórrala.",
    "¿Qué pasa con el issue del idioma?",
    { note: true, speak: false },
    "Prepárame un plan para migrar la configuración a TOML, paso a paso.",
    "¿Y cuánto tiempo crees que llevaría?",
    { text: "Perfecto. Lanza la migración del primer módulo y avísame.", ask: "long" },
    "¿Hay algo más pendiente antes de que me vaya a comer?",
    "Explícame con detalle por qué falló el último deploy.",
    { text: "Vale, gracias, eso es todo por ahora.", speak: "optional" },
    "Una última cosa: ¿cómo veo los logs del servicio?",
  ] },
  e: { name: "Notas sin palabras", turns: [
    "¿Qué es un mutex?",
    { note: true, speak: false },
    { note: true, speak: false },
    "¿Y un semáforo?",
    { note: true, speak: false },
  ] },
};

export const variants = {
  pointer: "See the Sidevoice instructions.",
  reinforced: "See the Sidevoice instructions. Voice: self-contained, one point at a time.",
};
