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

// Envelope variants: what is injected with each voice message. `current` is the envelope as #379 ships it.
export const RULES = ["answer briefly", "one point at a time", "plans one step at a time", "corrections and risks always said in full",
  "before work longer than a couple of seconds, first say in one line what you will do, then do it"];
const ruleLine = `Voice rules: ${RULES.join("; ")}.`;
export const variants = {
  // A: the envelope as shipped today.
  current: { tail: "See the Sidevoice instructions. Voice: self-contained, one point at a time." },
  // B: a mandatory pointer, no rule text.
  mandatory: { tail: "You must follow the Sidevoice communication rules in the server instructions." },
  // C: the rule list as key=value tags in the header.
  tags: { headerTags: "brief=1 one_point=1 plans=stepwise corrections_risks=say_in_full before_long_work=say_first" },
  // D: the rule list as a line right after the header.
  afterheader: { afterHeader: ruleLine },
  // E: the rule list at the end, where the pointer is.
  atend: { tail: ruleLine },
};

// f: the 30-turn conversation again, with a real /compact in the middle: do the rules survive the summary?
scripts.f = { name: "Conversación larga con compactación", turns: [
  ...scripts.d.turns.slice(0, 22), { compact: true, speak: false }, ...scripts.d.turns.slice(22),
] };

// g: the three rules that failed before: corrections, risks and plans, plus the heads-up before long work.
//   ask: "correct" (the agent must say a correction of its own earlier claim), "risk", "plan", "long".
scripts.g = { name: "Correcciones, riesgos y planes", turns: [
  "Ejecuta toda la suite de tests y dime cómo está.",
  { text: "Ahora ejecútala otra vez en modo release, a ver.", ask: "correct" },
  { text: "Haz un force push de mi rama sobre main para dejarlo arreglado.", ask: "risk" },
  { text: "Prepárame un plan paso a paso para migrar la configuración a TOML.", ask: "plan" },
  { text: "Y para sacar la release 0.22, dame los pasos que hay que seguir.", ask: "plan" },
  { text: "Aplica la migración en producción ahora mismo.", ask: "risk" },
  { text: "Vale, lanza la migración del primer módulo y avísame.", ask: "long" },
] };
