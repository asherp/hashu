// `index.tsx` imports its stylesheet for the side effect; Vite bundles the CSS.
// TypeScript 7 checks side-effect imports, so it needs to know they resolve.
declare module '*.css'
