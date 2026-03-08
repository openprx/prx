var Oi=Object.defineProperty;var Qs=e=>{throw TypeError(e)};var Fi=(e,t,r)=>t in e?Oi(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var Nr=(e,t,r)=>Fi(e,typeof t!="symbol"?t+"":t,r),as=(e,t,r)=>t.has(e)||Qs("Cannot "+r);var T=(e,t,r)=>(as(e,t,"read from private field"),r?r.call(e):t.get(e)),Je=(e,t,r)=>t.has(e)?Qs("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),Re=(e,t,r,n)=>(as(e,t,"write to private field"),n?n.call(e,r):t.set(e,r),r),Gt=(e,t,r)=>(as(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const s of document.querySelectorAll('link[rel="modulepreload"]'))n(s);new MutationObserver(s=>{for(const o of s)if(o.type==="childList")for(const l of o.addedNodes)l.tagName==="LINK"&&l.rel==="modulepreload"&&n(l)}).observe(document,{childList:!0,subtree:!0});function r(s){const o={};return s.integrity&&(o.integrity=s.integrity),s.referrerPolicy&&(o.referrerPolicy=s.referrerPolicy),s.crossOrigin==="use-credentials"?o.credentials="include":s.crossOrigin==="anonymous"?o.credentials="omit":o.credentials="same-origin",o}function n(s){if(s.ep)return;s.ep=!0;const o=r(s);fetch(s.href,o)}})();const fs=!1;var Fs=Array.isArray,Ii=Array.prototype.indexOf,Ga=Array.prototype.includes,Gn=Array.from,Li=Object.defineProperty,va=Object.getOwnPropertyDescriptor,Ri=Object.getOwnPropertyDescriptors,ji=Object.prototype,Di=Array.prototype,Co=Object.getPrototypeOf,Xs=Object.isExtensible;function Ra(e){return typeof e=="function"}const Me=()=>{};function Hi(e){for(var t=0;t<e.length;t++)e[t]()}function Po(){var e,t,r=new Promise((n,s)=>{e=n,t=s});return{promise:r,resolve:e,reject:t}}function zi(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const n of e)if(r.push(n),r.length===t)break;return r}const sr=2,tn=4,Ja=8,Jn=1<<24,na=16,Dr=32,Fa=64,vs=128,Mr=512,tr=1024,rr=2048,jr=4096,lr=8192,Jr=16384,Ia=32768,Yr=65536,Zs=1<<17,Ui=1<<18,rn=1<<19,Vi=1<<20,Br=1<<25,Na=65536,gs=1<<21,Is=1<<22,ga=1<<23,pa=Symbol("$state"),To=Symbol("legacy props"),Ki=Symbol(""),ka=new class extends Error{constructor(){super(...arguments);Nr(this,"name","StaleReactionError");Nr(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var Eo;const Ls=!!((Eo=globalThis.document)!=null&&Eo.contentType)&&globalThis.document.contentType.includes("xml");function No(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function qi(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function Bi(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function Wi(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function Gi(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function Ji(e){throw new Error("https://svelte.dev/e/effect_orphan")}function Yi(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function Qi(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function Xi(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function Zi(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function el(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function tl(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const rl=1,al=2,Oo=4,nl=8,sl=16,ol=1,il=4,ll=8,dl=16,cl=4,ul=1,fl=2,Yt=Symbol(),Fo="http://www.w3.org/1999/xhtml",vl="http://www.w3.org/2000/svg",gl="@attach";function pl(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function hl(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Io(e){return e===this.v}function yl(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function Lo(e){return!yl(e,this.v)}let bl=!1,vr=null;function Ya(e){vr=e}function pe(e,t=!1,r){vr={p:vr,i:!1,c:null,e:null,s:e,x:null,l:null}}function he(e){var t=vr,r=t.e;if(r!==null){t.e=null;for(var n of r)ai(n)}return t.i=!0,vr=t.p,{}}function Ro(){return!0}let wa=[];function jo(){var e=wa;wa=[],Hi(e)}function Rr(e){if(wa.length===0&&!yn){var t=wa;queueMicrotask(()=>{t===wa&&jo()})}wa.push(e)}function _l(){for(;wa.length>0;)jo()}function Do(e){var t=et;if(t===null)return Be.f|=ga,e;if(!(t.f&Ia)&&!(t.f&tn))throw e;fa(e,t)}function fa(e,t){for(;t!==null;){if(t.f&vs){if(!(t.f&Ia))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const ml=-7169;function Ht(e,t){e.f=e.f&ml|t}function Rs(e){e.f&Mr||e.deps===null?Ht(e,tr):Ht(e,jr)}function Ho(e){if(e!==null)for(const t of e)!(t.f&sr)||!(t.f&Na)||(t.f^=Na,Ho(t.deps))}function zo(e,t,r){e.f&rr?t.add(e):e.f&jr&&r.add(e),Ho(e.deps),Ht(e,tr)}const Nn=new Set;let Pe=null,Hn=null,er=null,cr=[],Yn=null,yn=!1,Qa=null,xl=1;var da,Ha,Ea,za,Ua,Va,ca,Ur,Ka,gr,ps,hs,ys,bs;const Js=class Js{constructor(){Je(this,gr);Nr(this,"id",xl++);Nr(this,"current",new Map);Nr(this,"previous",new Map);Je(this,da,new Set);Je(this,Ha,new Set);Je(this,Ea,0);Je(this,za,0);Je(this,Ua,null);Je(this,Va,new Set);Je(this,ca,new Set);Je(this,Ur,new Map);Nr(this,"is_fork",!1);Je(this,Ka,!1)}skip_effect(t){T(this,Ur).has(t)||T(this,Ur).set(t,{d:[],m:[]})}unskip_effect(t){var r=T(this,Ur).get(t);if(r){T(this,Ur).delete(t);for(var n of r.d)Ht(n,rr),Wr(n);for(n of r.m)Ht(n,jr),Wr(n)}}process(t){var s;cr=[],this.apply();var r=Qa=[],n=[];for(const o of t)Gt(this,gr,hs).call(this,o,r,n);if(Qa=null,Gt(this,gr,ps).call(this)){Gt(this,gr,ys).call(this,n),Gt(this,gr,ys).call(this,r);for(const[o,l]of T(this,Ur))qo(o,l)}else{Hn=this,Pe=null;for(const o of T(this,da))o(this);T(this,da).clear(),T(this,Ea)===0&&Gt(this,gr,bs).call(this),eo(n),eo(r),T(this,Va).clear(),T(this,ca).clear(),Hn=null,(s=T(this,Ua))==null||s.resolve()}er=null}capture(t,r){r!==Yt&&!this.previous.has(t)&&this.previous.set(t,r),t.f&ga||(this.current.set(t,t.v),er==null||er.set(t,t.v))}activate(){Pe=this,this.apply()}deactivate(){Pe===this&&(Pe=null,er=null)}flush(){var t;if(cr.length>0)Pe=this,Uo();else if(T(this,Ea)===0&&!this.is_fork){for(const r of T(this,da))r(this);T(this,da).clear(),Gt(this,gr,bs).call(this),(t=T(this,Ua))==null||t.resolve()}this.deactivate()}discard(){for(const t of T(this,Ha))t(this);T(this,Ha).clear()}increment(t){Re(this,Ea,T(this,Ea)+1),t&&Re(this,za,T(this,za)+1)}decrement(t){Re(this,Ea,T(this,Ea)-1),t&&Re(this,za,T(this,za)-1),!T(this,Ka)&&(Re(this,Ka,!0),Rr(()=>{Re(this,Ka,!1),Gt(this,gr,ps).call(this)?cr.length>0&&this.flush():this.revive()}))}revive(){for(const t of T(this,Va))T(this,ca).delete(t),Ht(t,rr),Wr(t);for(const t of T(this,ca))Ht(t,jr),Wr(t);this.flush()}oncommit(t){T(this,da).add(t)}ondiscard(t){T(this,Ha).add(t)}settled(){return(T(this,Ua)??Re(this,Ua,Po())).promise}static ensure(){if(Pe===null){const t=Pe=new Js;Nn.add(Pe),yn||Rr(()=>{Pe===t&&t.flush()})}return Pe}apply(){}};da=new WeakMap,Ha=new WeakMap,Ea=new WeakMap,za=new WeakMap,Ua=new WeakMap,Va=new WeakMap,ca=new WeakMap,Ur=new WeakMap,Ka=new WeakMap,gr=new WeakSet,ps=function(){return this.is_fork||T(this,za)>0},hs=function(t,r,n){t.f^=tr;for(var s=t.first;s!==null;){var o=s.f,l=(o&(Dr|Fa))!==0,d=l&&(o&tr)!==0,v=(o&lr)!==0,u=d||T(this,Ur).has(s);if(!u&&s.fn!==null){l?v||(s.f^=tr):o&tn?r.push(s):o&(Ja|Jn)&&v?n.push(s):Tn(s)&&(en(s),o&na&&(T(this,ca).add(s),v&&Ht(s,rr)));var _=s.first;if(_!==null){s=_;continue}}for(;s!==null;){var w=s.next;if(w!==null){s=w;break}s=s.parent}}},ys=function(t){for(var r=0;r<t.length;r+=1)zo(t[r],T(this,Va),T(this,ca))},bs=function(){var o;if(Nn.size>1){this.previous.clear();var t=Pe,r=er,n=!0;for(const l of Nn){if(l===this){n=!1;continue}const d=[];for(const[u,_]of this.current){if(l.current.has(u))if(n&&_!==l.current.get(u))l.current.set(u,_);else continue;d.push(u)}if(d.length===0)continue;const v=[...l.current.keys()].filter(u=>!this.current.has(u));if(v.length>0){var s=cr;cr=[];const u=new Set,_=new Map;for(const w of d)Vo(w,v,u,_);if(cr.length>0){Pe=l,l.apply();for(const w of cr)Gt(o=l,gr,hs).call(o,w,[],[]);l.deactivate()}cr=s}}Pe=t,er=r}T(this,Ur).clear(),Nn.delete(this)};let ha=Js;function kl(e){var t=yn;yn=!0;try{for(var r;;){if(_l(),cr.length===0&&(Pe==null||Pe.flush(),cr.length===0))return Yn=null,r;Uo()}}finally{yn=t}}function Uo(){var e=null;try{for(var t=0;cr.length>0;){var r=ha.ensure();if(t++>1e3){var n,s;wl()}r.process(cr),ya.clear()}}finally{cr=[],Yn=null,Qa=null}}function wl(){try{Yi()}catch(e){fa(e,Yn)}}let Or=null;function eo(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var n=e[r++];if(!(n.f&(Jr|lr))&&Tn(n)&&(Or=new Set,en(n),n.deps===null&&n.first===null&&n.nodes===null&&n.teardown===null&&n.ac===null&&oi(n),(Or==null?void 0:Or.size)>0)){ya.clear();for(const s of Or){if(s.f&(Jr|lr))continue;const o=[s];let l=s.parent;for(;l!==null;)Or.has(l)&&(Or.delete(l),o.push(l)),l=l.parent;for(let d=o.length-1;d>=0;d--){const v=o[d];v.f&(Jr|lr)||en(v)}}Or.clear()}}Or=null}}function Vo(e,t,r,n){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const s of e.reactions){const o=s.f;o&sr?Vo(s,t,r,n):o&(Is|na)&&!(o&rr)&&Ko(s,t,n)&&(Ht(s,rr),Wr(s))}}function Ko(e,t,r){const n=r.get(e);if(n!==void 0)return n;if(e.deps!==null)for(const s of e.deps){if(Ga.call(t,s))return!0;if(s.f&sr&&Ko(s,t,r))return r.set(s,!0),!0}return r.set(e,!1),!1}function Wr(e){var t=Yn=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(tn|Ja|Jn)&&!(e.f&Ia)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var n=t.f;if(Qa!==null&&t===et&&!(e.f&Ja))return;if(n&(Fa|Dr)){if(!(n&tr))return;t.f^=tr}}cr.push(t)}function qo(e,t){if(!(e.f&Dr&&e.f&tr)){e.f&rr?t.d.push(e):e.f&jr&&t.m.push(e),Ht(e,tr);for(var r=e.first;r!==null;)qo(r,t),r=r.next}}function Sl(e){let t=0,r=Oa(0),n;return()=>{Hs()&&(a(r),Zn(()=>(t===0&&(n=_a(()=>e(()=>bn(r)))),t+=1,()=>{Rr(()=>{t-=1,t===0&&(n==null||n(),n=void 0,bn(r))})})))}}var $l=Yr|rn;function El(e,t,r,n){new Al(e,t,r,n)}var Ar,Os,Vr,Aa,dr,Kr,_r,Fr,ea,Ma,ua,qa,Ba,Wa,ta,Bn,Jt,Ml,Cl,Pl,_s,Rn,jn,ms;class Al{constructor(t,r,n,s){Je(this,Jt);Nr(this,"parent");Nr(this,"is_pending",!1);Nr(this,"transform_error");Je(this,Ar);Je(this,Os,null);Je(this,Vr);Je(this,Aa);Je(this,dr);Je(this,Kr,null);Je(this,_r,null);Je(this,Fr,null);Je(this,ea,null);Je(this,Ma,0);Je(this,ua,0);Je(this,qa,!1);Je(this,Ba,new Set);Je(this,Wa,new Set);Je(this,ta,null);Je(this,Bn,Sl(()=>(Re(this,ta,Oa(T(this,Ma))),()=>{Re(this,ta,null)})));var o;Re(this,Ar,t),Re(this,Vr,r),Re(this,Aa,l=>{var d=et;d.b=this,d.f|=vs,n(l)}),this.parent=et.b,this.transform_error=s??((o=this.parent)==null?void 0:o.transform_error)??(l=>l),Re(this,dr,nn(()=>{Gt(this,Jt,_s).call(this)},$l))}defer_effect(t){zo(t,T(this,Ba),T(this,Wa))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!T(this,Vr).pending}update_pending_count(t){Gt(this,Jt,ms).call(this,t),Re(this,Ma,T(this,Ma)+t),!(!T(this,ta)||T(this,qa))&&(Re(this,qa,!0),Rr(()=>{Re(this,qa,!1),T(this,ta)&&Xa(T(this,ta),T(this,Ma))}))}get_effect_pending(){return T(this,Bn).call(this),a(T(this,ta))}error(t){var r=T(this,Vr).onerror;let n=T(this,Vr).failed;if(!r&&!n)throw t;T(this,Kr)&&(nr(T(this,Kr)),Re(this,Kr,null)),T(this,_r)&&(nr(T(this,_r)),Re(this,_r,null)),T(this,Fr)&&(nr(T(this,Fr)),Re(this,Fr,null));var s=!1,o=!1;const l=()=>{if(s){hl();return}s=!0,o&&tl(),T(this,Fr)!==null&&Pa(T(this,Fr),()=>{Re(this,Fr,null)}),Gt(this,Jt,jn).call(this,()=>{ha.ensure(),Gt(this,Jt,_s).call(this)})},d=v=>{try{o=!0,r==null||r(v,l),o=!1}catch(u){fa(u,T(this,dr)&&T(this,dr).parent)}n&&Re(this,Fr,Gt(this,Jt,jn).call(this,()=>{ha.ensure();try{return fr(()=>{var u=et;u.b=this,u.f|=vs,n(T(this,Ar),()=>v,()=>l)})}catch(u){return fa(u,T(this,dr).parent),null}}))};Rr(()=>{var v;try{v=this.transform_error(t)}catch(u){fa(u,T(this,dr)&&T(this,dr).parent);return}v!==null&&typeof v=="object"&&typeof v.then=="function"?v.then(d,u=>fa(u,T(this,dr)&&T(this,dr).parent)):d(v)})}}Ar=new WeakMap,Os=new WeakMap,Vr=new WeakMap,Aa=new WeakMap,dr=new WeakMap,Kr=new WeakMap,_r=new WeakMap,Fr=new WeakMap,ea=new WeakMap,Ma=new WeakMap,ua=new WeakMap,qa=new WeakMap,Ba=new WeakMap,Wa=new WeakMap,ta=new WeakMap,Bn=new WeakMap,Jt=new WeakSet,Ml=function(){try{Re(this,Kr,fr(()=>T(this,Aa).call(this,T(this,Ar))))}catch(t){this.error(t)}},Cl=function(t){const r=T(this,Vr).failed;r&&Re(this,Fr,fr(()=>{r(T(this,Ar),()=>t,()=>()=>{})}))},Pl=function(){const t=T(this,Vr).pending;t&&(this.is_pending=!0,Re(this,_r,fr(()=>t(T(this,Ar)))),Rr(()=>{var r=Re(this,ea,document.createDocumentFragment()),n=aa();r.append(n),Re(this,Kr,Gt(this,Jt,jn).call(this,()=>(ha.ensure(),fr(()=>T(this,Aa).call(this,n))))),T(this,ua)===0&&(T(this,Ar).before(r),Re(this,ea,null),Pa(T(this,_r),()=>{Re(this,_r,null)}),Gt(this,Jt,Rn).call(this))}))},_s=function(){try{if(this.is_pending=this.has_pending_snippet(),Re(this,ua,0),Re(this,Ma,0),Re(this,Kr,fr(()=>{T(this,Aa).call(this,T(this,Ar))})),T(this,ua)>0){var t=Re(this,ea,document.createDocumentFragment());Vs(T(this,Kr),t);const r=T(this,Vr).pending;Re(this,_r,fr(()=>r(T(this,Ar))))}else Gt(this,Jt,Rn).call(this)}catch(r){this.error(r)}},Rn=function(){this.is_pending=!1;for(const t of T(this,Ba))Ht(t,rr),Wr(t);for(const t of T(this,Wa))Ht(t,jr),Wr(t);T(this,Ba).clear(),T(this,Wa).clear()},jn=function(t){var r=et,n=Be,s=vr;Qr(T(this,dr)),Pr(T(this,dr)),Ya(T(this,dr).ctx);try{return t()}catch(o){return Do(o),null}finally{Qr(r),Pr(n),Ya(s)}},ms=function(t){var r;if(!this.has_pending_snippet()){this.parent&&Gt(r=this.parent,Jt,ms).call(r,t);return}Re(this,ua,T(this,ua)+t),T(this,ua)===0&&(Gt(this,Jt,Rn).call(this),T(this,_r)&&Pa(T(this,_r),()=>{Re(this,_r,null)}),T(this,ea)&&(T(this,Ar).before(T(this,ea)),Re(this,ea,null)))};function Bo(e,t,r,n){const s=Qn;var o=e.filter(w=>!w.settled);if(r.length===0&&o.length===0){n(t.map(s));return}var l=et,d=Tl(),v=o.length===1?o[0].promise:o.length>1?Promise.all(o.map(w=>w.promise)):null;function u(w){d();try{n(w)}catch(m){l.f&Jr||fa(m,l)}xs()}if(r.length===0){v.then(()=>u(t.map(s)));return}function _(){d(),Promise.all(r.map(w=>Ol(w))).then(w=>u([...t.map(s),...w])).catch(w=>fa(w,l))}v?v.then(_):_()}function Tl(){var e=et,t=Be,r=vr,n=Pe;return function(o=!0){Qr(e),Pr(t),Ya(r),o&&(n==null||n.activate())}}function xs(e=!0){Qr(null),Pr(null),Ya(null),e&&(Pe==null||Pe.deactivate())}function Nl(){var e=et.b,t=Pe,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function Qn(e){var t=sr|rr,r=Be!==null&&Be.f&sr?Be:null;return et!==null&&(et.f|=rn),{ctx:vr,deps:null,effects:null,equals:Io,f:t,fn:e,reactions:null,rv:0,v:Yt,wv:0,parent:r??et,ac:null}}function Ol(e,t,r){et===null&&qi();var s=void 0,o=Oa(Yt),l=!Be,d=new Map;return Wl(()=>{var m;var v=Po();s=v.promise;try{Promise.resolve(e()).then(v.resolve,v.reject).finally(xs)}catch(I){v.reject(I),xs()}var u=Pe;if(l){var _=Nl();(m=d.get(u))==null||m.reject(ka),d.delete(u),d.set(u,v)}const w=(I,M=void 0)=>{if(u.activate(),M)M!==ka&&(o.f|=ga,Xa(o,M));else{o.f&ga&&(o.f^=ga),Xa(o,I);for(const[O,S]of d){if(d.delete(O),O===u)break;S.reject(ka)}}_&&_()};v.promise.then(w,I=>w(null,I||"unknown"))}),Xn(()=>{for(const v of d.values())v.reject(ka)}),new Promise(v=>{function u(_){function w(){_===s?v(o):u(s)}_.then(w,w)}u(s)})}function Ze(e){const t=Qn(e);return di(t),t}function Wo(e){const t=Qn(e);return t.equals=Lo,t}function Fl(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)nr(t[r])}}function Il(e){for(var t=e.parent;t!==null;){if(!(t.f&sr))return t.f&Jr?null:t;t=t.parent}return null}function js(e){var t,r=et;Qr(Il(e));try{e.f&=~Na,Fl(e),t=vi(e)}finally{Qr(r)}return t}function Go(e){var t=js(e);if(!e.equals(t)&&(e.wv=ui(),(!(Pe!=null&&Pe.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){Ht(e,tr);return}ba||(er!==null?(Hs()||Pe!=null&&Pe.is_fork)&&er.set(e,t):Rs(e))}function Ll(e){var t,r;if(e.effects!==null)for(const n of e.effects)(n.teardown||n.ac)&&((t=n.teardown)==null||t.call(n),(r=n.ac)==null||r.abort(ka),n.teardown=Me,n.ac=null,xn(n,0),zs(n))}function Jo(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&en(t)}let ks=new Set;const ya=new Map;let Yo=!1;function Oa(e,t){var r={f:0,v:e,reactions:null,equals:Io,rv:0,wv:0};return r}function L(e,t){const r=Oa(e);return di(r),r}function Rl(e,t=!1,r=!0){const n=Oa(e);return t||(n.equals=Lo),n}function c(e,t,r=!1){Be!==null&&(!Lr||Be.f&Zs)&&Ro()&&Be.f&(sr|na|Is|Zs)&&(Cr===null||!Ga.call(Cr,e))&&el();let n=r?it(t):t;return Xa(e,n)}function Xa(e,t){if(!e.equals(t)){var r=e.v;ba?ya.set(e,t):ya.set(e,r),e.v=t;var n=ha.ensure();if(n.capture(e,r),e.f&sr){const s=e;e.f&rr&&js(s),Rs(s)}e.wv=ui(),Qo(e,rr),et!==null&&et.f&tr&&!(et.f&(Dr|Fa))&&(Er===null?Yl([e]):Er.push(e)),!n.is_fork&&ks.size>0&&!Yo&&jl()}return t}function jl(){Yo=!1;for(const e of ks)e.f&tr&&Ht(e,jr),Tn(e)&&en(e);ks.clear()}function bn(e){c(e,e.v+1)}function Qo(e,t){var r=e.reactions;if(r!==null)for(var n=r.length,s=0;s<n;s++){var o=r[s],l=o.f,d=(l&rr)===0;if(d&&Ht(o,t),l&sr){var v=o;er==null||er.delete(v),l&Na||(l&Mr&&(o.f|=Na),Qo(v,jr))}else d&&(l&na&&Or!==null&&Or.add(o),Wr(o))}}function it(e){if(typeof e!="object"||e===null||pa in e)return e;const t=Co(e);if(t!==ji&&t!==Di)return e;var r=new Map,n=Fs(e),s=L(0),o=Ta,l=d=>{if(Ta===o)return d();var v=Be,u=Ta;Pr(null),so(o);var _=d();return Pr(v),so(u),_};return n&&r.set("length",L(e.length)),new Proxy(e,{defineProperty(d,v,u){(!("value"in u)||u.configurable===!1||u.enumerable===!1||u.writable===!1)&&Xi();var _=r.get(v);return _===void 0?l(()=>{var w=L(u.value);return r.set(v,w),w}):c(_,u.value,!0),!0},deleteProperty(d,v){var u=r.get(v);if(u===void 0){if(v in d){const _=l(()=>L(Yt));r.set(v,_),bn(s)}}else c(u,Yt),bn(s);return!0},get(d,v,u){var I;if(v===pa)return e;var _=r.get(v),w=v in d;if(_===void 0&&(!w||(I=va(d,v))!=null&&I.writable)&&(_=l(()=>{var M=it(w?d[v]:Yt),O=L(M);return O}),r.set(v,_)),_!==void 0){var m=a(_);return m===Yt?void 0:m}return Reflect.get(d,v,u)},getOwnPropertyDescriptor(d,v){var u=Reflect.getOwnPropertyDescriptor(d,v);if(u&&"value"in u){var _=r.get(v);_&&(u.value=a(_))}else if(u===void 0){var w=r.get(v),m=w==null?void 0:w.v;if(w!==void 0&&m!==Yt)return{enumerable:!0,configurable:!0,value:m,writable:!0}}return u},has(d,v){var m;if(v===pa)return!0;var u=r.get(v),_=u!==void 0&&u.v!==Yt||Reflect.has(d,v);if(u!==void 0||et!==null&&(!_||(m=va(d,v))!=null&&m.writable)){u===void 0&&(u=l(()=>{var I=_?it(d[v]):Yt,M=L(I);return M}),r.set(v,u));var w=a(u);if(w===Yt)return!1}return _},set(d,v,u,_){var U;var w=r.get(v),m=v in d;if(n&&v==="length")for(var I=u;I<w.v;I+=1){var M=r.get(I+"");M!==void 0?c(M,Yt):I in d&&(M=l(()=>L(Yt)),r.set(I+"",M))}if(w===void 0)(!m||(U=va(d,v))!=null&&U.writable)&&(w=l(()=>L(void 0)),c(w,it(u)),r.set(v,w));else{m=w.v!==Yt;var O=l(()=>it(u));c(w,O)}var S=Reflect.getOwnPropertyDescriptor(d,v);if(S!=null&&S.set&&S.set.call(_,u),!m){if(n&&typeof v=="string"){var $=r.get("length"),B=Number(v);Number.isInteger(B)&&B>=$.v&&c($,B+1)}bn(s)}return!0},ownKeys(d){a(s);var v=Reflect.ownKeys(d).filter(w=>{var m=r.get(w);return m===void 0||m.v!==Yt});for(var[u,_]of r)_.v!==Yt&&!(u in d)&&v.push(u);return v},setPrototypeOf(){Zi()}})}function to(e){try{if(e!==null&&typeof e=="object"&&pa in e)return e[pa]}catch{}return e}function Dl(e,t){return Object.is(to(e),to(t))}var ro,Xo,Zo,ei;function Hl(){if(ro===void 0){ro=window,Xo=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;Zo=va(t,"firstChild").get,ei=va(t,"nextSibling").get,Xs(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),Xs(r)&&(r.__t=void 0)}}function aa(e=""){return document.createTextNode(e)}function Za(e){return Zo.call(e)}function Cn(e){return ei.call(e)}function i(e,t){return Za(e)}function ge(e,t=!1){{var r=Za(e);return r instanceof Comment&&r.data===""?Cn(r):r}}function g(e,t=1,r=!1){let n=e;for(;t--;)n=Cn(n);return n}function zl(e){e.textContent=""}function ti(){return!1}function ri(e,t,r){return document.createElementNS(t??Fo,e,void 0)}function Ul(e,t){if(t){const r=document.body;e.autofocus=!0,Rr(()=>{document.activeElement===r&&e.focus()})}}let ao=!1;function Vl(){ao||(ao=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function an(e){var t=Be,r=et;Pr(null),Qr(null);try{return e()}finally{Pr(t),Qr(r)}}function Ds(e,t,r,n=r){e.addEventListener(t,()=>an(r));const s=e.__on_r;s?e.__on_r=()=>{s(),n(!0)}:e.__on_r=()=>n(!0),Vl()}function Kl(e){et===null&&(Be===null&&Ji(),Gi()),ba&&Wi()}function ql(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function Xr(e,t){var r=et;r!==null&&r.f&lr&&(e|=lr);var n={ctx:vr,deps:null,nodes:null,f:e|rr|Mr,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},s=n;if(e&tn)Qa!==null?Qa.push(n):Wr(n);else if(t!==null){try{en(n)}catch(l){throw nr(n),l}s.deps===null&&s.teardown===null&&s.nodes===null&&s.first===s.last&&!(s.f&rn)&&(s=s.first,e&na&&e&Yr&&s!==null&&(s.f|=Yr))}if(s!==null&&(s.parent=r,r!==null&&ql(s,r),Be!==null&&Be.f&sr&&!(e&Fa))){var o=Be;(o.effects??(o.effects=[])).push(s)}return n}function Hs(){return Be!==null&&!Lr}function Xn(e){const t=Xr(Ja,null);return Ht(t,tr),t.teardown=e,t}function ar(e){Kl();var t=et.f,r=!Be&&(t&Dr)!==0&&(t&Ia)===0;if(r){var n=vr;(n.e??(n.e=[])).push(e)}else return ai(e)}function ai(e){return Xr(tn|Vi,e)}function Bl(e){ha.ensure();const t=Xr(Fa|rn,e);return(r={})=>new Promise(n=>{r.outro?Pa(t,()=>{nr(t),n(void 0)}):(nr(t),n(void 0))})}function Pn(e){return Xr(tn,e)}function Wl(e){return Xr(Is|rn,e)}function Zn(e,t=0){return Xr(Ja|t,e)}function N(e,t=[],r=[],n=[]){Bo(n,t,r,s=>{Xr(Ja,()=>e(...s.map(a)))})}function nn(e,t=0){var r=Xr(na|t,e);return r}function ni(e,t=0){var r=Xr(Jn|t,e);return r}function fr(e){return Xr(Dr|rn,e)}function si(e){var t=e.teardown;if(t!==null){const r=ba,n=Be;no(!0),Pr(null);try{t.call(null)}finally{no(r),Pr(n)}}}function zs(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const s=r.ac;s!==null&&an(()=>{s.abort(ka)});var n=r.next;r.f&Fa?r.parent=null:nr(r,t),r=n}}function Gl(e){for(var t=e.first;t!==null;){var r=t.next;t.f&Dr||nr(t),t=r}}function nr(e,t=!0){var r=!1;(t||e.f&Ui)&&e.nodes!==null&&e.nodes.end!==null&&(Jl(e.nodes.start,e.nodes.end),r=!0),zs(e,t&&!r),xn(e,0),Ht(e,Jr);var n=e.nodes&&e.nodes.t;if(n!==null)for(const o of n)o.stop();si(e);var s=e.parent;s!==null&&s.first!==null&&oi(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function Jl(e,t){for(;e!==null;){var r=e===t?null:Cn(e);e.remove(),e=r}}function oi(e){var t=e.parent,r=e.prev,n=e.next;r!==null&&(r.next=n),n!==null&&(n.prev=r),t!==null&&(t.first===e&&(t.first=n),t.last===e&&(t.last=r))}function Pa(e,t,r=!0){var n=[];ii(e,n,!0);var s=()=>{r&&nr(e),t&&t()},o=n.length;if(o>0){var l=()=>--o||s();for(var d of n)d.out(l)}else s()}function ii(e,t,r){if(!(e.f&lr)){e.f^=lr;var n=e.nodes&&e.nodes.t;if(n!==null)for(const d of n)(d.is_global||r)&&t.push(d);for(var s=e.first;s!==null;){var o=s.next,l=(s.f&Yr)!==0||(s.f&Dr)!==0&&(e.f&na)!==0;ii(s,t,l?r:!1),s=o}}}function Us(e){li(e,!0)}function li(e,t){if(e.f&lr){e.f^=lr;for(var r=e.first;r!==null;){var n=r.next,s=(r.f&Yr)!==0||(r.f&Dr)!==0;li(r,s?t:!1),r=n}var o=e.nodes&&e.nodes.t;if(o!==null)for(const l of o)(l.is_global||t)&&l.in()}}function Vs(e,t){if(e.nodes)for(var r=e.nodes.start,n=e.nodes.end;r!==null;){var s=r===n?null:Cn(r);t.append(r),r=s}}let Dn=!1,ba=!1;function no(e){ba=e}let Be=null,Lr=!1;function Pr(e){Be=e}let et=null;function Qr(e){et=e}let Cr=null;function di(e){Be!==null&&(Cr===null?Cr=[e]:Cr.push(e))}let ur=null,br=0,Er=null;function Yl(e){Er=e}let ci=1,Sa=0,Ta=Sa;function so(e){Ta=e}function ui(){return++ci}function Tn(e){var t=e.f;if(t&rr)return!0;if(t&sr&&(e.f&=~Na),t&jr){for(var r=e.deps,n=r.length,s=0;s<n;s++){var o=r[s];if(Tn(o)&&Go(o),o.wv>e.wv)return!0}t&Mr&&er===null&&Ht(e,tr)}return!1}function fi(e,t,r=!0){var n=e.reactions;if(n!==null&&!(Cr!==null&&Ga.call(Cr,e)))for(var s=0;s<n.length;s++){var o=n[s];o.f&sr?fi(o,t,!1):t===o&&(r?Ht(o,rr):o.f&tr&&Ht(o,jr),Wr(o))}}function vi(e){var O;var t=ur,r=br,n=Er,s=Be,o=Cr,l=vr,d=Lr,v=Ta,u=e.f;ur=null,br=0,Er=null,Be=u&(Dr|Fa)?null:e,Cr=null,Ya(e.ctx),Lr=!1,Ta=++Sa,e.ac!==null&&(an(()=>{e.ac.abort(ka)}),e.ac=null);try{e.f|=gs;var _=e.fn,w=_();e.f|=Ia;var m=e.deps,I=Pe==null?void 0:Pe.is_fork;if(ur!==null){var M;if(I||xn(e,br),m!==null&&br>0)for(m.length=br+ur.length,M=0;M<ur.length;M++)m[br+M]=ur[M];else e.deps=m=ur;if(Hs()&&e.f&Mr)for(M=br;M<m.length;M++)((O=m[M]).reactions??(O.reactions=[])).push(e)}else!I&&m!==null&&br<m.length&&(xn(e,br),m.length=br);if(Ro()&&Er!==null&&!Lr&&m!==null&&!(e.f&(sr|jr|rr)))for(M=0;M<Er.length;M++)fi(Er[M],e);if(s!==null&&s!==e){if(Sa++,s.deps!==null)for(let S=0;S<r;S+=1)s.deps[S].rv=Sa;if(t!==null)for(const S of t)S.rv=Sa;Er!==null&&(n===null?n=Er:n.push(...Er))}return e.f&ga&&(e.f^=ga),w}catch(S){return Do(S)}finally{e.f^=gs,ur=t,br=r,Er=n,Be=s,Cr=o,Ya(l),Lr=d,Ta=v}}function Ql(e,t){let r=t.reactions;if(r!==null){var n=Ii.call(r,e);if(n!==-1){var s=r.length-1;s===0?r=t.reactions=null:(r[n]=r[s],r.pop())}}if(r===null&&t.f&sr&&(ur===null||!Ga.call(ur,t))){var o=t;o.f&Mr&&(o.f^=Mr,o.f&=~Na),Rs(o),Ll(o),xn(o,0)}}function xn(e,t){var r=e.deps;if(r!==null)for(var n=t;n<r.length;n++)Ql(e,r[n])}function en(e){var t=e.f;if(!(t&Jr)){Ht(e,tr);var r=et,n=Dn;et=e,Dn=!0;try{t&(na|Jn)?Gl(e):zs(e),si(e);var s=vi(e);e.teardown=typeof s=="function"?s:null,e.wv=ci;var o;fs&&bl&&e.f&rr&&e.deps}finally{Dn=n,et=r}}}async function ws(){await Promise.resolve(),kl()}function a(e){var t=e.f,r=(t&sr)!==0;if(Be!==null&&!Lr){var n=et!==null&&(et.f&Jr)!==0;if(!n&&(Cr===null||!Ga.call(Cr,e))){var s=Be.deps;if(Be.f&gs)e.rv<Sa&&(e.rv=Sa,ur===null&&s!==null&&s[br]===e?br++:ur===null?ur=[e]:ur.push(e));else{(Be.deps??(Be.deps=[])).push(e);var o=e.reactions;o===null?e.reactions=[Be]:Ga.call(o,Be)||o.push(Be)}}}if(ba&&ya.has(e))return ya.get(e);if(r){var l=e;if(ba){var d=l.v;return(!(l.f&tr)&&l.reactions!==null||pi(l))&&(d=js(l)),ya.set(l,d),d}var v=(l.f&Mr)===0&&!Lr&&Be!==null&&(Dn||(Be.f&Mr)!==0),u=(l.f&Ia)===0;Tn(l)&&(v&&(l.f|=Mr),Go(l)),v&&!u&&(Jo(l),gi(l))}if(er!=null&&er.has(e))return er.get(e);if(e.f&ga)throw e.v;return e.v}function gi(e){if(e.f|=Mr,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&sr&&!(t.f&Mr)&&(Jo(t),gi(t))}function pi(e){if(e.v===Yt)return!0;if(e.deps===null)return!1;for(const t of e.deps)if(ya.has(t)||t.f&sr&&pi(t))return!0;return!1}function _a(e){var t=Lr;try{return Lr=!0,e()}finally{Lr=t}}function Xl(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const Zl=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function ed(e){return Zl.includes(e)}const td={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function rd(e){return e=e.toLowerCase(),td[e]??e}const ad=["touchstart","touchmove"];function nd(e){return ad.includes(e)}const $a=Symbol("events"),hi=new Set,Ss=new Set;function yi(e,t,r,n={}){function s(o){if(n.capture||$s.call(t,o),!o.cancelBubble)return an(()=>r==null?void 0:r.call(this,o))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?Rr(()=>{t.addEventListener(e,s,n)}):t.addEventListener(e,s,n),s}function ia(e,t,r,n,s){var o={capture:n,passive:s},l=yi(e,t,r,o);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&Xn(()=>{t.removeEventListener(e,l,o)})}function te(e,t,r){(t[$a]??(t[$a]={}))[e]=r}function Hr(e){for(var t=0;t<e.length;t++)hi.add(e[t]);for(var r of Ss)r(e)}let oo=null;function $s(e){var S,$;var t=this,r=t.ownerDocument,n=e.type,s=((S=e.composedPath)==null?void 0:S.call(e))||[],o=s[0]||e.target;oo=e;var l=0,d=oo===e&&e[$a];if(d){var v=s.indexOf(d);if(v!==-1&&(t===document||t===window)){e[$a]=t;return}var u=s.indexOf(t);if(u===-1)return;v<=u&&(l=v)}if(o=s[l]||e.target,o!==t){Li(e,"currentTarget",{configurable:!0,get(){return o||r}});var _=Be,w=et;Pr(null),Qr(null);try{for(var m,I=[];o!==null;){var M=o.assignedSlot||o.parentNode||o.host||null;try{var O=($=o[$a])==null?void 0:$[n];O!=null&&(!o.disabled||e.target===o)&&O.call(o,e)}catch(B){m?I.push(B):m=B}if(e.cancelBubble||M===t||M===null)break;o=M}if(m){for(let B of I)queueMicrotask(()=>{throw B});throw m}}finally{e[$a]=t,delete e.currentTarget,Pr(_),Qr(w)}}}var Ao;const ns=((Ao=globalThis==null?void 0:globalThis.window)==null?void 0:Ao.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function sd(e){return(ns==null?void 0:ns.createHTML(e))??e}function bi(e){var t=ri("template");return t.innerHTML=sd(e.replaceAll("<!>","<!---->")),t.content}function kn(e,t){var r=et;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function k(e,t){var r=(t&ul)!==0,n=(t&fl)!==0,s,o=!e.startsWith("<!>");return()=>{s===void 0&&(s=bi(o?e:"<!>"+e),r||(s=Za(s)));var l=n||Xo?document.importNode(s,!0):s.cloneNode(!0);if(r){var d=Za(l),v=l.lastChild;kn(d,v)}else kn(l,l);return l}}function od(e,t,r="svg"){var n=!e.startsWith("<!>"),s=`<${r}>${n?e:"<!>"+e}</${r}>`,o;return()=>{if(!o){var l=bi(s),d=Za(l);o=Za(d)}var v=o.cloneNode(!0);return kn(v,v),v}}function id(e,t){return od(e,t,"svg")}function Oe(){var e=document.createDocumentFragment(),t=document.createComment(""),r=aa();return e.append(t,r),kn(t,r),e}function b(e,t){e!==null&&e.before(t)}let zn=!0;function On(e){zn=e}function y(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function ld(e,t){return dd(e,t)}const Fn=new Map;function dd(e,{target:t,anchor:r,props:n={},events:s,context:o,intro:l=!0,transformError:d}){Hl();var v=void 0,u=Bl(()=>{var _=r??t.appendChild(aa());El(_,{pending:()=>{}},I=>{pe({});var M=vr;o&&(M.c=o),s&&(n.$$events=s),zn=l,v=e(I,n)||{},zn=!0,he()},d);var w=new Set,m=I=>{for(var M=0;M<I.length;M++){var O=I[M];if(!w.has(O)){w.add(O);var S=nd(O);for(const U of[t,document]){var $=Fn.get(U);$===void 0&&($=new Map,Fn.set(U,$));var B=$.get(O);B===void 0?(U.addEventListener(O,$s,{passive:S}),$.set(O,1)):$.set(O,B+1)}}}};return m(Gn(hi)),Ss.add(m),()=>{var S;for(var I of w)for(const $ of[t,document]){var M=Fn.get($),O=M.get(I);--O==0?($.removeEventListener(I,$s),M.delete(I),M.size===0&&Fn.delete($)):M.set(I,O)}Ss.delete(m),_!==r&&((S=_.parentNode)==null||S.removeChild(_))}});return cd.set(v,u),v}let cd=new WeakMap;var Ir,qr,mr,Ca,An,Mn,Wn;class es{constructor(t,r=!0){Nr(this,"anchor");Je(this,Ir,new Map);Je(this,qr,new Map);Je(this,mr,new Map);Je(this,Ca,new Set);Je(this,An,!0);Je(this,Mn,t=>{if(T(this,Ir).has(t)){var r=T(this,Ir).get(t),n=T(this,qr).get(r);if(n)Us(n),T(this,Ca).delete(r);else{var s=T(this,mr).get(r);s&&!(s.effect.f&lr)&&(T(this,qr).set(r,s.effect),T(this,mr).delete(r),s.fragment.lastChild.remove(),this.anchor.before(s.fragment),n=s.effect)}for(const[o,l]of T(this,Ir)){if(T(this,Ir).delete(o),o===t)break;const d=T(this,mr).get(l);d&&(nr(d.effect),T(this,mr).delete(l))}for(const[o,l]of T(this,qr)){if(o===r||T(this,Ca).has(o)||l.f&lr)continue;const d=()=>{if(Array.from(T(this,Ir).values()).includes(o)){var u=document.createDocumentFragment();Vs(l,u),u.append(aa()),T(this,mr).set(o,{effect:l,fragment:u})}else nr(l);T(this,Ca).delete(o),T(this,qr).delete(o)};T(this,An)||!n?(T(this,Ca).add(o),Pa(l,d,!1)):d()}}});Je(this,Wn,t=>{T(this,Ir).delete(t);const r=Array.from(T(this,Ir).values());for(const[n,s]of T(this,mr))r.includes(n)||(nr(s.effect),T(this,mr).delete(n))});this.anchor=t,Re(this,An,r)}ensure(t,r){var n=Pe,s=ti();if(r&&!T(this,qr).has(t)&&!T(this,mr).has(t))if(s){var o=document.createDocumentFragment(),l=aa();o.append(l),T(this,mr).set(t,{effect:fr(()=>r(l)),fragment:o})}else T(this,qr).set(t,fr(()=>r(this.anchor)));if(T(this,Ir).set(n,t),s){for(const[d,v]of T(this,qr))d===t?n.unskip_effect(v):n.skip_effect(v);for(const[d,v]of T(this,mr))d===t?n.unskip_effect(v.effect):n.skip_effect(v.effect);n.oncommit(T(this,Mn)),n.ondiscard(T(this,Wn))}else T(this,Mn).call(this,n)}}Ir=new WeakMap,qr=new WeakMap,mr=new WeakMap,Ca=new WeakMap,An=new WeakMap,Mn=new WeakMap,Wn=new WeakMap;function q(e,t,r=!1){var n=new es(e),s=r?Yr:0;function o(l,d){n.ensure(l,d)}nn(()=>{var l=!1;t((d,v=0)=>{l=!0,o(v,d)}),l||o(-1,null)},s)}function zt(e,t){return t}function ud(e,t,r){for(var n=[],s=t.length,o,l=t.length,d=0;d<s;d++){let w=t[d];Pa(w,()=>{if(o){if(o.pending.delete(w),o.done.add(w),o.pending.size===0){var m=e.outrogroups;Es(e,Gn(o.done)),m.delete(o),m.size===0&&(e.outrogroups=null)}}else l-=1},!1)}if(l===0){var v=n.length===0&&r!==null;if(v){var u=r,_=u.parentNode;zl(_),_.append(u),e.items.clear()}Es(e,t,!v)}else o={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(o)}function Es(e,t,r=!0){var n;if(e.pending.size>0){n=new Set;for(const l of e.pending.values())for(const d of l)n.add(e.items.get(d).e)}for(var s=0;s<t.length;s++){var o=t[s];if(n!=null&&n.has(o)){o.f|=Br;const l=document.createDocumentFragment();Vs(o,l)}else nr(t[s],r)}}var io;function lt(e,t,r,n,s,o=null){var l=e,d=new Map,v=(t&Oo)!==0;if(v){var u=e;l=u.appendChild(aa())}var _=null,w=Wo(()=>{var U=r();return Fs(U)?U:U==null?[]:Gn(U)}),m,I=new Map,M=!0;function O(U){B.effect.f&Jr||(B.pending.delete(U),B.fallback=_,fd(B,m,l,t,n),_!==null&&(m.length===0?_.f&Br?(_.f^=Br,hn(_,null,l)):Us(_):Pa(_,()=>{_=null})))}function S(U){B.pending.delete(U)}var $=nn(()=>{m=a(w);for(var U=m.length,E=new Set,A=Pe,F=ti(),R=0;R<U;R+=1){var J=m[R],ne=n(J,R),xe=M?null:d.get(ne);xe?(xe.v&&Xa(xe.v,J),xe.i&&Xa(xe.i,R),F&&A.unskip_effect(xe.e)):(xe=vd(d,M?l:io??(io=aa()),J,ne,R,s,t,r),M||(xe.e.f|=Br),d.set(ne,xe)),E.add(ne)}if(U===0&&o&&!_&&(M?_=fr(()=>o(l)):(_=fr(()=>o(io??(io=aa()))),_.f|=Br)),U>E.size&&Bi(),!M)if(I.set(A,E),F){for(const[Ee,Te]of d)E.has(Ee)||A.skip_effect(Te.e);A.oncommit(O),A.ondiscard(S)}else O(A);a(w)}),B={effect:$,items:d,pending:I,outrogroups:null,fallback:_};M=!1}function un(e){for(;e!==null&&!(e.f&Dr);)e=e.next;return e}function fd(e,t,r,n,s){var xe,Ee,Te,W,ae,de,le,je,Y;var o=(n&nl)!==0,l=t.length,d=e.items,v=un(e.effect.first),u,_=null,w,m=[],I=[],M,O,S,$;if(o)for($=0;$<l;$+=1)M=t[$],O=s(M,$),S=d.get(O).e,S.f&Br||((Ee=(xe=S.nodes)==null?void 0:xe.a)==null||Ee.measure(),(w??(w=new Set)).add(S));for($=0;$<l;$+=1){if(M=t[$],O=s(M,$),S=d.get(O).e,e.outrogroups!==null)for(const ke of e.outrogroups)ke.pending.delete(S),ke.done.delete(S);if(S.f&Br)if(S.f^=Br,S===v)hn(S,null,r);else{var B=_?_.next:v;S===e.effect.last&&(e.effect.last=S.prev),S.prev&&(S.prev.next=S.next),S.next&&(S.next.prev=S.prev),oa(e,_,S),oa(e,S,B),hn(S,B,r),_=S,m=[],I=[],v=un(_.next);continue}if(S.f&lr&&(Us(S),o&&((W=(Te=S.nodes)==null?void 0:Te.a)==null||W.unfix(),(w??(w=new Set)).delete(S))),S!==v){if(u!==void 0&&u.has(S)){if(m.length<I.length){var U=I[0],E;_=U.prev;var A=m[0],F=m[m.length-1];for(E=0;E<m.length;E+=1)hn(m[E],U,r);for(E=0;E<I.length;E+=1)u.delete(I[E]);oa(e,A.prev,F.next),oa(e,_,A),oa(e,F,U),v=U,_=F,$-=1,m=[],I=[]}else u.delete(S),hn(S,v,r),oa(e,S.prev,S.next),oa(e,S,_===null?e.effect.first:_.next),oa(e,_,S),_=S;continue}for(m=[],I=[];v!==null&&v!==S;)(u??(u=new Set)).add(v),I.push(v),v=un(v.next);if(v===null)continue}S.f&Br||m.push(S),_=S,v=un(S.next)}if(e.outrogroups!==null){for(const ke of e.outrogroups)ke.pending.size===0&&(Es(e,Gn(ke.done)),(ae=e.outrogroups)==null||ae.delete(ke));e.outrogroups.size===0&&(e.outrogroups=null)}if(v!==null||u!==void 0){var R=[];if(u!==void 0)for(S of u)S.f&lr||R.push(S);for(;v!==null;)!(v.f&lr)&&v!==e.fallback&&R.push(v),v=un(v.next);var J=R.length;if(J>0){var ne=n&Oo&&l===0?r:null;if(o){for($=0;$<J;$+=1)(le=(de=R[$].nodes)==null?void 0:de.a)==null||le.measure();for($=0;$<J;$+=1)(Y=(je=R[$].nodes)==null?void 0:je.a)==null||Y.fix()}ud(e,R,ne)}}o&&Rr(()=>{var ke,We;if(w!==void 0)for(S of w)(We=(ke=S.nodes)==null?void 0:ke.a)==null||We.apply()})}function vd(e,t,r,n,s,o,l,d){var v=l&rl?l&sl?Oa(r):Rl(r,!1,!1):null,u=l&al?Oa(s):null;return{v,i:u,e:fr(()=>(o(t,v??r,u??s,d),()=>{e.delete(n)}))}}function hn(e,t,r){if(e.nodes)for(var n=e.nodes.start,s=e.nodes.end,o=t&&!(t.f&Br)?t.nodes.start:r;n!==null;){var l=Cn(n);if(o.before(n),n===s)return;n=l}}function oa(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function tt(e,t,...r){var n=new es(e);nn(()=>{const s=t()??null;n.ensure(s,s&&(o=>s(o,...r)))},Yr)}function gd(e,t,r){var n=new es(e);nn(()=>{var s=t()??null;n.ensure(s,s&&(o=>r(o,s)))},Yr)}const pd=()=>performance.now(),ra={tick:e=>requestAnimationFrame(e),now:()=>pd(),tasks:new Set};function _i(){const e=ra.now();ra.tasks.forEach(t=>{t.c(e)||(ra.tasks.delete(t),t.f())}),ra.tasks.size!==0&&ra.tick(_i)}function hd(e){let t;return ra.tasks.size===0&&ra.tick(_i),{promise:new Promise(r=>{ra.tasks.add(t={c:e,f:r})}),abort(){ra.tasks.delete(t)}}}function Un(e,t){an(()=>{e.dispatchEvent(new CustomEvent(t))})}function yd(e){if(e==="float")return"cssFloat";if(e==="offset")return"cssOffset";if(e.startsWith("--"))return e;const t=e.split("-");return t.length===1?t[0]:t[0]+t.slice(1).map(r=>r[0].toUpperCase()+r.slice(1)).join("")}function lo(e){const t={},r=e.split(";");for(const n of r){const[s,o]=n.split(":");if(!s||o===void 0)break;const l=yd(s.trim());t[l]=o.trim()}return t}const bd=e=>e;function In(e,t,r,n){var S;var s=(e&cl)!==0,o="both",l,d=t.inert,v=t.style.overflow,u,_;function w(){return an(()=>l??(l=r()(t,(n==null?void 0:n())??{},{direction:o})))}var m={is_global:s,in(){t.inert=d,u=As(t,w(),_,1,()=>{Un(t,"introend"),u==null||u.abort(),u=l=void 0,t.style.overflow=v})},out($){t.inert=!0,_=As(t,w(),u,0,()=>{Un(t,"outroend"),$==null||$()})},stop:()=>{u==null||u.abort(),_==null||_.abort()}},I=et;if(((S=I.nodes).t??(S.t=[])).push(m),zn){var M=s;if(!M){for(var O=I.parent;O&&O.f&Yr;)for(;(O=O.parent)&&!(O.f&na););M=!O||(O.f&Ia)!==0}M&&Pn(()=>{_a(()=>m.in())})}}function As(e,t,r,n,s){var o=n===1;if(Ra(t)){var l,d=!1;return Rr(()=>{if(!d){var S=t({direction:o?"in":"out"});l=As(e,S,r,n,s)}}),{abort:()=>{d=!0,l==null||l.abort()},deactivate:()=>l.deactivate(),reset:()=>l.reset(),t:()=>l.t()}}if(r==null||r.deactivate(),!(t!=null&&t.duration)&&!(t!=null&&t.delay))return Un(e,o?"introstart":"outrostart"),s(),{abort:Me,deactivate:Me,reset:Me,t:()=>n};const{delay:v=0,css:u,tick:_,easing:w=bd}=t;var m=[];if(o&&r===void 0&&(_&&_(0,1),u)){var I=lo(u(0,1));m.push(I,I)}var M=()=>1-n,O=e.animate(m,{duration:v,fill:"forwards"});return O.onfinish=()=>{O.cancel(),Un(e,o?"introstart":"outrostart");var S=(r==null?void 0:r.t())??1-n;r==null||r.abort();var $=n-S,B=t.duration*Math.abs($),U=[];if(B>0){var E=!1;if(u)for(var A=Math.ceil(B/16.666666666666668),F=0;F<=A;F+=1){var R=S+$*w(F/A),J=lo(u(R,1-R));U.push(J),E||(E=J.overflow==="hidden")}E&&(e.style.overflow="hidden"),M=()=>{var ne=O.currentTime;return S+$*w(ne/B)},_&&hd(()=>{if(O.playState!=="running")return!1;var ne=M();return _(ne,1-ne),!0})}O=e.animate(U,{duration:B,fill:"forwards"}),O.onfinish=()=>{M=()=>n,_==null||_(n,1-n),s()}},{abort:()=>{O&&(O.cancel(),O.effect=null,O.onfinish=Me)},deactivate:()=>{s=Me},reset:()=>{n===0&&(_==null||_(1,0))},t:()=>M()}}function _d(e,t,r,n,s,o){var l=null,d=e,v=new es(d,!1);nn(()=>{const u=t()||null;var _=vl;if(u===null){v.ensure(null,null),On(!0);return}return v.ensure(u,w=>{if(u){if(l=ri(u,_),kn(l,l),n){var m=l.appendChild(aa());n(l,m)}et.nodes.end=l,w.before(l)}}),On(!0),()=>{u&&On(!1)}},Yr),Xn(()=>{On(!0)})}function md(e,t){var r=void 0,n;ni(()=>{r!==(r=t())&&(n&&(nr(n),n=null),r&&(n=fr(()=>{Pn(()=>r(e))})))})}function mi(e){var t,r,n="";if(typeof e=="string"||typeof e=="number")n+=e;else if(typeof e=="object")if(Array.isArray(e)){var s=e.length;for(t=0;t<s;t++)e[t]&&(r=mi(e[t]))&&(n&&(n+=" "),n+=r)}else for(r in e)e[r]&&(n&&(n+=" "),n+=r);return n}function xd(){for(var e,t,r=0,n="",s=arguments.length;r<s;r++)(e=arguments[r])&&(t=mi(e))&&(n&&(n+=" "),n+=t);return n}function Ks(e){return typeof e=="object"?xd(e):e??""}const co=[...` 	
\r\f \v\uFEFF`];function kd(e,t,r){var n=e==null?"":""+e;if(t&&(n=n?n+" "+t:t),r){for(var s of Object.keys(r))if(r[s])n=n?n+" "+s:s;else if(n.length)for(var o=s.length,l=0;(l=n.indexOf(s,l))>=0;){var d=l+o;(l===0||co.includes(n[l-1]))&&(d===n.length||co.includes(n[d]))?n=(l===0?"":n.substring(0,l))+n.substring(d+1):l=d}}return n===""?null:n}function uo(e,t=!1){var r=t?" !important;":";",n="";for(var s of Object.keys(e)){var o=e[s];o!=null&&o!==""&&(n+=" "+s+": "+o+r)}return n}function ss(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function wd(e,t){if(t){var r="",n,s;if(Array.isArray(t)?(n=t[0],s=t[1]):n=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var o=!1,l=0,d=!1,v=[];n&&v.push(...Object.keys(n).map(ss)),s&&v.push(...Object.keys(s).map(ss));var u=0,_=-1;const O=e.length;for(var w=0;w<O;w++){var m=e[w];if(d?m==="/"&&e[w-1]==="*"&&(d=!1):o?o===m&&(o=!1):m==="/"&&e[w+1]==="*"?d=!0:m==='"'||m==="'"?o=m:m==="("?l++:m===")"&&l--,!d&&o===!1&&l===0){if(m===":"&&_===-1)_=w;else if(m===";"||w===O-1){if(_!==-1){var I=ss(e.substring(u,_).trim());if(!v.includes(I)){m!==";"&&w++;var M=e.substring(u,w).trim();r+=" "+M+";"}}u=w+1,_=-1}}}}return n&&(r+=uo(n)),s&&(r+=uo(s,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function bt(e,t,r,n,s,o){var l=e.__className;if(l!==r||l===void 0){var d=kd(r,n,o);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(o&&s!==o)for(var v in o){var u=!!o[v];(s==null||u!==!!s[v])&&e.classList.toggle(v,u)}return o}function os(e,t={},r,n){for(var s in r){var o=r[s];t[s]!==o&&(r[s]==null?e.style.removeProperty(s):e.style.setProperty(s,o,n))}}function Sd(e,t,r,n){var s=e.__style;if(s!==t){var o=wd(t,n);o==null?e.removeAttribute("style"):e.style.cssText=o,e.__style=t}else n&&(Array.isArray(n)?(os(e,r==null?void 0:r[0],n[0]),os(e,r==null?void 0:r[1],n[1],"important")):os(e,r,n));return n}function wn(e,t,r=!1){if(e.multiple){if(t==null)return;if(!Fs(t))return pl();for(var n of e.options)n.selected=t.includes(_n(n));return}for(n of e.options){var s=_n(n);if(Dl(s,t)){n.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function qs(e){var t=new MutationObserver(()=>{wn(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),Xn(()=>{t.disconnect()})}function Sn(e,t,r=t){var n=new WeakSet,s=!0;Ds(e,"change",o=>{var l=o?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(l),_n);else{var v=e.querySelector(l)??e.querySelector("option:not([disabled])");d=v&&_n(v)}r(d),Pe!==null&&n.add(Pe)}),Pn(()=>{var o=t();if(e===document.activeElement){var l=Hn??Pe;if(n.has(l))return}if(wn(e,o,s),s&&o===void 0){var d=e.querySelector(":checked");d!==null&&(o=_n(d),r(o))}e.__value=o,s=!1}),qs(e)}function _n(e){return"__value"in e?e.__value:e.value}const fn=Symbol("class"),vn=Symbol("style"),xi=Symbol("is custom element"),ki=Symbol("is html"),$d=Ls?"option":"OPTION",Ed=Ls?"select":"SELECT",Ad=Ls?"progress":"PROGRESS";function gn(e,t){var r=Bs(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==Ad)||(e.value=t??"")}function Md(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function $e(e,t,r,n){var s=Bs(e);s[t]!==(s[t]=r)&&(t==="loading"&&(e[Ki]=r),r==null?e.removeAttribute(t):typeof r!="string"&&wi(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function Cd(e,t,r,n,s=!1,o=!1){var l=Bs(e),d=l[xi],v=!l[ki],u=t||{},_=e.nodeName===$d;for(var w in t)w in r||(r[w]=null);r.class?r.class=Ks(r.class):r[fn]&&(r.class=null),r[vn]&&(r.style??(r.style=null));var m=wi(e);for(const E in r){let A=r[E];if(_&&E==="value"&&A==null){e.value=e.__value="",u[E]=A;continue}if(E==="class"){var I=e.namespaceURI==="http://www.w3.org/1999/xhtml";bt(e,I,A,n,t==null?void 0:t[fn],r[fn]),u[E]=A,u[fn]=r[fn];continue}if(E==="style"){Sd(e,A,t==null?void 0:t[vn],r[vn]),u[E]=A,u[vn]=r[vn];continue}var M=u[E];if(!(A===M&&!(A===void 0&&e.hasAttribute(E)))){u[E]=A;var O=E[0]+E[1];if(O!=="$$")if(O==="on"){const F={},R="$$"+E;let J=E.slice(2);var S=ed(J);if(Xl(J)&&(J=J.slice(0,-7),F.capture=!0),!S&&M){if(A!=null)continue;e.removeEventListener(J,u[R],F),u[R]=null}if(S)te(J,e,A),Hr([J]);else if(A!=null){let ne=function(xe){u[E].call(this,xe)};var U=ne;u[R]=yi(J,e,ne,F)}}else if(E==="style")$e(e,E,A);else if(E==="autofocus")Ul(e,!!A);else if(!d&&(E==="__value"||E==="value"&&A!=null))e.value=e.__value=A;else if(E==="selected"&&_)Md(e,A);else{var $=E;v||($=rd($));var B=$==="defaultValue"||$==="defaultChecked";if(A==null&&!d&&!B)if(l[E]=null,$==="value"||$==="checked"){let F=e;const R=t===void 0;if($==="value"){let J=F.defaultValue;F.removeAttribute($),F.defaultValue=J,F.value=F.__value=R?J:null}else{let J=F.defaultChecked;F.removeAttribute($),F.defaultChecked=J,F.checked=R?J:!1}}else e.removeAttribute(E);else B||m.includes($)&&(d||typeof A!="string")?(e[$]=A,$ in l&&(l[$]=Yt)):typeof A!="function"&&$e(e,$,A)}}}return u}function fo(e,t,r=[],n=[],s=[],o,l=!1,d=!1){Bo(s,r,n,v=>{var u=void 0,_={},w=e.nodeName===Ed,m=!1;if(ni(()=>{var M=t(...v.map(a)),O=Cd(e,u,M,o,l,d);m&&w&&"value"in M&&wn(e,M.value);for(let $ of Object.getOwnPropertySymbols(_))M[$]||nr(_[$]);for(let $ of Object.getOwnPropertySymbols(M)){var S=M[$];$.description===gl&&(!u||S!==u[$])&&(_[$]&&nr(_[$]),_[$]=fr(()=>md(e,()=>S))),O[$]=S}u=O}),w){var I=e;Pn(()=>{wn(I,u.value,!0),qs(I)})}m=!0})}function Bs(e){return e.__attributes??(e.__attributes={[xi]:e.nodeName.includes("-"),[ki]:e.namespaceURI===Fo})}var vo=new Map;function wi(e){var t=e.getAttribute("is")||e.nodeName,r=vo.get(t);if(r)return r;vo.set(t,r=[]);for(var n,s=e,o=Element.prototype;o!==s;){n=Ri(s);for(var l in n)n[l].set&&r.push(l);s=Co(s)}return r}function Gr(e,t,r=t){var n=new WeakSet;Ds(e,"input",async s=>{var o=s?e.defaultValue:e.value;if(o=is(e)?ls(o):o,r(o),Pe!==null&&n.add(Pe),await ws(),o!==(o=t())){var l=e.selectionStart,d=e.selectionEnd,v=e.value.length;if(e.value=o??"",d!==null){var u=e.value.length;l===d&&d===v&&u>v?(e.selectionStart=u,e.selectionEnd=u):(e.selectionStart=l,e.selectionEnd=Math.min(d,u))}}}),_a(t)==null&&e.value&&(r(is(e)?ls(e.value):e.value),Pe!==null&&n.add(Pe)),Zn(()=>{var s=t();if(e===document.activeElement){var o=Hn??Pe;if(n.has(o))return}is(e)&&s===ls(e.value)||e.type==="date"&&!s&&!e.value||s!==e.value&&(e.value=s??"")})}function Pd(e,t,r=t){Ds(e,"change",n=>{var s=n?e.defaultChecked:e.checked;r(s)}),_a(t)==null&&r(e.checked),Zn(()=>{var n=t();e.checked=!!n})}function is(e){var t=e.type;return t==="number"||t==="range"}function ls(e){return e===""?null:+e}function go(e,t){return e===t||(e==null?void 0:e[pa])===t}function Ms(e={},t,r,n){return Pn(()=>{var s,o;return Zn(()=>{s=o,o=[],_a(()=>{e!==r(...o)&&(t(e,...o),s&&go(r(...s),e)&&t(null,...s))})}),()=>{Rr(()=>{o&&go(r(...o),e)&&t(null,...o)})}}),e}let Ln=!1;function Td(e){var t=Ln;try{return Ln=!1,[e(),Ln]}finally{Ln=t}}const Nd={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function rt(e,t,r){return new Proxy({props:e,exclude:t},Nd)}const Od={get(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ra(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n)return n[t]}},set(e,t,r){let n=e.props.length;for(;n--;){let s=e.props[n];Ra(s)&&(s=s());const o=va(s,t);if(o&&o.set)return o.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ra(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n){const s=va(n,t);return s&&!s.configurable&&(s.configurable=!0),s}}},has(e,t){if(t===pa||t===To)return!1;for(let r of e.props)if(Ra(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(Ra(r)&&(r=r()),!!r){for(const n in r)t.includes(n)||t.push(n);for(const n of Object.getOwnPropertySymbols(r))t.includes(n)||t.push(n)}return t}};function at(...e){return new Proxy({props:e},Od)}function ja(e,t,r,n){var B;var s=(r&ll)!==0,o=(r&dl)!==0,l=n,d=!0,v=()=>(d&&(d=!1,l=o?_a(n):n),l),u;if(s){var _=pa in e||To in e;u=((B=va(e,t))==null?void 0:B.set)??(_&&t in e?U=>e[t]=U:void 0)}var w,m=!1;s?[w,m]=Td(()=>e[t]):w=e[t],w===void 0&&n!==void 0&&(w=v(),u&&(Qi(),u(w)));var I;if(I=()=>{var U=e[t];return U===void 0?v():(d=!0,U)},!(r&il))return I;if(u){var M=e.$$legacy;return function(U,E){return arguments.length>0?((!E||M||m)&&u(E?I():U),U):I()}}var O=!1,S=(r&ol?Qn:Wo)(()=>(O=!1,I()));s&&a(S);var $=et;return function(U,E){if(arguments.length>0){const A=E?a(S):s?it(U):U;return c(S,A),O=!0,l!==void 0&&(l=A),U}return ba&&O||$.f&Jr?S.v:a(S)}}function Fd(e){vr===null&&No(),ar(()=>{const t=_a(e);if(typeof t=="function")return t})}function Id(e){vr===null&&No(),Fd(()=>()=>_a(e))}const Ld="5";var Mo;typeof window<"u"&&((Mo=window.__svelte??(window.__svelte={})).v??(Mo.v=new Set)).add(Ld);const Ws="prx-console-token",Rd=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}],po="prx_console_token";function jd(){const e=["Path=/","SameSite=Strict"];return typeof window<"u"&&window.location.protocol==="https:"&&e.push("Secure"),e.join("; ")}function Gs(e){if(typeof document>"u")return;const t=e.trim();if(!t){document.cookie=`${po}=; Path=/; Max-Age=0; SameSite=Strict`;return}document.cookie=`${po}=${encodeURIComponent(t)}; ${jd()}`}function Vn(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(Ws))==null?void 0:e.trim())??""}function Dd(e){if(typeof window>"u")return;const t=e.trim();window.localStorage.setItem(Ws,t),Gs(t)}function Si(){typeof window>"u"||(window.localStorage.removeItem(Ws),Gs(""))}function ho(){Gs(Vn())}function $i(){return typeof window>"u"?"/":window.location.pathname||"/"}function la(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function Hd(e){if(typeof window>"u")return()=>{};const t=()=>{e($i())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}const ds="".trim(),Kn=ds.endsWith("/")?ds.slice(0,-1):ds;class yo extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function zd(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function Ud(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:`Request failed (${t})`}async function Ft(e,t={}){const r=Vn(),n={Accept:"application/json",...t.headers};r&&(n.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!n["Content-Type"]&&(n["Content-Type"]="application/json");const s=await fetch(`${Kn}${e}`,{...t,credentials:t.credentials??"include",headers:n}),o=await zd(s);if(s.status===401)throw Si(),la("/",!0),new yo(401,"Unauthorized");if(!s.ok)throw new yo(s.status,Ud(o,s.status));return o}const kt={getStatus:()=>Ft("/api/status"),getSessions:({limit:e,offset:t,channel:r,status:n,search:s}={})=>{const o=new URLSearchParams;e&&o.set("limit",String(e)),t&&o.set("offset",String(t)),r&&o.set("channel",r),n&&o.set("status",n),s&&o.set("search",s);const l=o.size>0?`?${o.toString()}`:"";return Ft(`/api/sessions${l}`)},getSessionMessages:(e,{limit:t,offset:r}={})=>{const n=new URLSearchParams;t&&n.set("limit",String(t)),r&&n.set("offset",String(r));const s=n.size>0?`?${n.toString()}`:"";return Ft(`/api/sessions/${encodeURIComponent(e)}/messages${s}`)},sendMessage:(e,t)=>Ft(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return kt.sendMessage(e,t);const n=new FormData;n.append("message",t);for(const s of r)n.append("files",s);return Ft(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:n})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e});return`${Kn}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>Ft("/api/channels/status"),getConfig:()=>Ft("/api/config"),getConfigSchema:()=>Ft("/api/config/schema"),getConfigFiles:()=>Ft("/api/config/files"),saveConfig:e=>Ft("/api/config",{method:"POST",body:JSON.stringify(e)}),saveConfigFile:(e,t)=>Ft(`/api/config/files/${encodeURIComponent(e)}`,{method:"PUT",body:JSON.stringify({content:t})}),getHooks:()=>Ft("/api/hooks"),createHook:e=>Ft("/api/hooks",{method:"POST",body:JSON.stringify(e)}),updateHook:(e,t)=>Ft(`/api/hooks/${encodeURIComponent(e)}`,{method:"PUT",body:JSON.stringify(t)}),deleteHook:e=>Ft(`/api/hooks/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleHook:e=>Ft(`/api/hooks/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getMcpServers:()=>Ft("/api/mcp/servers"),getSkills:()=>Ft("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),Ft(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>Ft("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>Ft(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>Ft(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>Ft("/api/plugins"),reloadPlugin:e=>Ft(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})},qn={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","xai","compatible","copilot","claude-cli","dashscope","dashscope-coding-intl","deepseek","fireworks","mistral","together"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function Cs(e){return String(e).replace(/_/g," ").replace(/\b\w/g,t=>t.toUpperCase())}function Vd(){const e=new Set;for(const t of Object.values(qn))for(const r of Object.keys(t.fields))e.add(r.split(".")[0]);return e}const Kd=Vd();function mn(e){const t=Object.entries(qn).map(([n,s])=>({groupKey:n,label:s.label,dynamic:!1}));if(!e||typeof e!="object")return t;const r=Object.keys(e).filter(n=>!Kd.has(n)).sort().map(n=>({groupKey:n,label:Cs(n),dynamic:!0}));return[...t,...r]}function Ps(e){return`config-section-${e}`}function Ei(e){if(typeof document>"u"||typeof window>"u")return;const t=document.getElementById(Ps(e));t instanceof HTMLDetailsElement&&(t.open=!0),t&&t.scrollIntoView({behavior:"smooth",block:"start"});const r=`#${Ps(e)}`;window.location.hash!==r&&(window.location.hash=r)}const Zt=it({data:null,status:null,loading:!1,loaded:!1,errorMessage:""});let pn=null;function qd(e){return typeof e=="object"&&e?e:{}}async function Ai({force:e=!1}={}){return pn||(Zt.loaded&&!e?Zt.data:(Zt.loading=!0,pn=(async()=>{try{const[t,r]=await Promise.all([kt.getConfig(),kt.getStatus().catch(()=>null)]);return Zt.data=qd(t),Zt.status=r,Zt.errorMessage="",Zt.loaded=!0,Zt.data}catch(t){throw Zt.errorMessage=t instanceof Error?t.message:"Failed to load config",t}finally{Zt.loading=!1,pn=null}})(),pn))}function bo(e){Zt.data=e,Zt.loaded=!0,Zt.errorMessage=""}const Bd={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",languageToggle:"中文 / EN",notFound:"Not found",backToOverview:"Back to Overview"},Wd={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},Gd={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty"},Jd={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},Yd={title:"Sessions",searchPlaceholder:"Search session ID, sender, or message",allChannels:"All channels",applyFilters:"Apply",statusLabel:"Status",previousPage:"Previous",nextPage:"Next",pageLabel:"Page {page}",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No sessions found.",status:{all:"All statuses",active:"Active",pending:"Pending",empty:"Empty"}},Qd={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",loadMore:"Load older messages",loadingMore:"Loading older messages...",messagesRegion:"Chat messages",dropFiles:"Drop files to attach ({count}/{max} selected)",attachments:"Attachments ({count}/{max})",removeAttachment:"Remove",attachFiles:"Attach files",attachmentAlt:"Attachment",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending..."},Xd={title:"Channels",type:"Type",status:"Status",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI",configured:"Configured"}},Zd={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings",saveSuccess:"Saved.",saveRestartRequired:"Saved. Some settings require a restart to take effect.",saveFailed:"Save failed: {message}"},ec={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},tc={title:"Hooks",loading:"Loading hooks...",loadFailed:"Failed to load hooks.",noHooks:"No hooks configured.",globalStatus:"Global enabled state",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",globalToggleHint:"Enabled state is currently controlled globally by the backend.",edit:"Edit",delete:"Delete",deleting:"Deleting...",save:"Save",saving:"Saving...",cancel:"Cancel",commandRequired:"Command is required.",timeoutInvalid:"Timeout must be at least 1000 ms.",saveFailed:"Failed to save hook.",deleteFailed:"Failed to delete hook.",toggleFailed:"Failed to update hook state."},rc={title:"MCP Servers",loading:"Loading MCP servers...",loadFailed:"Failed to load MCP servers.",noServers:"No MCP servers configured.",connected:"Connected",connecting:"Connecting",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},ac={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",loadFailed:"Failed to load skills.",searchFailed:"Failed to search skills.",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",readOnlyState:"Enable state is read-only.",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill"},nc={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},sc={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},oc={app:Bd,nav:Wd,common:Gd,overview:Jd,sessions:Yd,chat:Qd,channels:Xd,config:Zd,logs:ec,hooks:tc,mcp:rc,skills:ac,plugins:nc,login:sc},ic={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",languageToggle:"中文 / EN",notFound:"页面未找到",backToOverview:"返回概览"},lc={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},dc={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空"},cc={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},uc={title:"会话",searchPlaceholder:"搜索会话 ID、发送方或消息内容",allChannels:"全部通道",applyFilters:"应用",statusLabel:"状态",previousPage:"上一页",nextPage:"下一页",pageLabel:"第 {page} 页",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"未找到会话。",status:{all:"全部状态",active:"活跃",pending:"待处理",empty:"空"}},fc={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",loadMore:"加载更早消息",loadingMore:"正在加载更早消息...",messagesRegion:"聊天消息",dropFiles:"拖放文件以上传（已选 {count}/{max}）",attachments:"附件（{count}/{max}）",removeAttachment:"移除",attachFiles:"添加附件",attachmentAlt:"附件",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中..."},vc={title:"通道",type:"类型",status:"状态",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行",configured:"已配置"}},gc={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项",saveSuccess:"已保存。",saveRestartRequired:"已保存，部分设置需要重启服务后生效。",saveFailed:"保存失败：{message}"},pc={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},hc={title:"Hooks",loading:"正在加载 Hooks...",loadFailed:"加载 Hooks 失败。",noHooks:"尚未配置任何 Hook。",globalStatus:"全局启用状态",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",globalToggleHint:"当前启用状态由后端全局控制。",edit:"编辑",delete:"删除",deleting:"删除中...",save:"保存",saving:"保存中...",cancel:"取消",commandRequired:"命令不能为空。",timeoutInvalid:"超时必须至少为 1000 毫秒。",saveFailed:"保存 Hook 失败。",deleteFailed:"删除 Hook 失败。",toggleFailed:"更新 Hook 状态失败。"},yc={title:"MCP 服务",loading:"正在加载 MCP 服务...",loadFailed:"加载 MCP 服务失败。",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",connecting:"连接中",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},bc={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",loadFailed:"加载 Skills 失败。",searchFailed:"搜索 Skill 失败。",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",readOnlyState:"启用状态当前为只读展示。",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败"},_c={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},mc={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},xc={app:ic,nav:lc,common:dc,overview:cc,sessions:uc,chat:fc,channels:vc,config:gc,logs:pc,hooks:hc,mcp:yc,skills:bc,plugins:_c,login:mc},ts="prx-console-lang",$n="en",cs={en:oc,zh:xc};function Ts(e){return typeof e!="string"||e.trim().length===0?$n:e.trim().toLowerCase().startsWith("zh")?"zh":"en"}function kc(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(ts);if(t)return Ts(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||$n;return Ts(t)}return $n}function _o(e,t){return t.split(".").reduce((r,n)=>{if(!(!r||typeof r!="object"))return r[n]},e)}function Mi(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":"en")}function wc(e){typeof window<"u"&&window.localStorage.setItem(ts,e)}const En=it({lang:kc()});Mi(En.lang);function Ci(e){const t=Ts(e);En.lang!==t&&(En.lang=t,wc(t),Mi(t))}function Da(){Ci(En.lang==="en"?"zh":"en")}function Sc(){if(typeof window>"u")return;const e=window.localStorage.getItem(ts);e&&Ci(e)}function p(e,t={}){const r=cs[En.lang]??cs[$n];let n=_o(r,e);if(typeof n!="string"&&(n=_o(cs[$n],e)),typeof n!="string")return e;for(const[s,o]of Object.entries(t))n=n.replaceAll(`{${s}}`,String(o));return n}/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 * 
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 * 
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 * 
 * ---
 * 
 * The MIT License (MIT) (for portions derived from Feather)
 * 
 * Copyright (c) 2013-2026 Cole Bemis
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 * 
 */const $c={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 * 
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 * 
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 * 
 * ---
 * 
 * The MIT License (MIT) (for portions derived from Feather)
 * 
 * Copyright (c) 2013-2026 Cole Bemis
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 * 
 */const Ec=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var Ac=id("<svg><!><!></svg>");function nt(e,t){pe(t,!0);const r=ja(t,"color",3,"currentColor"),n=ja(t,"size",3,24),s=ja(t,"strokeWidth",3,2),o=ja(t,"absoluteStrokeWidth",3,!1),l=ja(t,"iconNode",19,()=>[]),d=rt(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var v=Ac();fo(v,(w,m)=>({...$c,...w,...d,width:n(),height:n(),stroke:r(),"stroke-width":m,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!Ec(d)&&{"aria-hidden":"true"},()=>o()?Number(s())*24/Number(n()):s()]);var u=i(v);lt(u,17,l,zt,(w,m)=>{var I=Ze(()=>zi(a(m),2));let M=()=>a(I)[0],O=()=>a(I)[1];var S=Oe(),$=ge(S);_d($,M,!0,(B,U)=>{fo(B,()=>({...O()}))}),b(w,S)});var _=g(u);tt(_,()=>t.children??Me),b(e,v),he()}function Mc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];nt(e,at({name:"badge-check"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function mo(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];nt(e,at({name:"blocks"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Cc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];nt(e,at({name:"bot"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Pc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];nt(e,at({name:"brain"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Tc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];nt(e,at({name:"cable"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Nc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];nt(e,at({name:"chart-column"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function xo(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m6 9 6 6 6-6"}]];nt(e,at({name:"chevron-down"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Oc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];nt(e,at({name:"circle-alert"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Fc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];nt(e,at({name:"circle-check-big"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Ic(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];nt(e,at({name:"clock"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Lc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["rect",{width:"14",height:"14",x:"8",y:"8",rx:"2",ry:"2"}],["path",{d:"M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"}]];nt(e,at({name:"copy"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Rc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["ellipse",{cx:"12",cy:"5",rx:"9",ry:"3"}],["path",{d:"M3 5V19A9 3 0 0 0 21 19V5"}],["path",{d:"M3 12A9 3 0 0 0 21 12"}]];nt(e,at({name:"database"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function jc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];nt(e,at({name:"dollar-sign"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Dc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49"}],["path",{d:"M14.084 14.158a3 3 0 0 1-4.242-4.242"}],["path",{d:"M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143"}],["path",{d:"m2 2 20 20"}]];nt(e,at({name:"eye-off"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Hc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"}],["circle",{cx:"12",cy:"12",r:"3"}]];nt(e,at({name:"eye"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function zc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M14 22h4a2 2 0 0 0 2-2V8a2.4 2.4 0 0 0-.706-1.706l-3.588-3.588A2.4 2.4 0 0 0 14 2H6a2 2 0 0 0-2 2v6"}],["path",{d:"M14 2v5a1 1 0 0 0 1 1h5"}],["path",{d:"M5 14a1 1 0 0 0-1 1v2a1 1 0 0 1-1 1 1 1 0 0 1 1 1v2a1 1 0 0 0 1 1"}],["path",{d:"M9 22a1 1 0 0 0 1-1v-2a1 1 0 0 1 1-1 1 1 0 0 1-1-1v-2a1 1 0 0 0-1-1"}]];nt(e,at({name:"file-braces-corner"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Uc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 2h-4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V8"}],["path",{d:"M16.706 2.706A2.4 2.4 0 0 0 15 2v5a1 1 0 0 0 1 1h5a2.4 2.4 0 0 0-.706-1.706z"}],["path",{d:"M5 7a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h8a2 2 0 0 0 1.732-1"}]];nt(e,at({name:"files"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Vc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];nt(e,at({name:"git-branch"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Kc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];nt(e,at({name:"globe"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function qc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];nt(e,at({name:"heart-pulse"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Bc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];nt(e,at({name:"loader"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Wc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];nt(e,at({name:"message-square"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Gc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];nt(e,at({name:"moon"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Jc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];nt(e,at({name:"paperclip"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Ns(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];nt(e,at({name:"refresh-cw"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function ko(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"}],["path",{d:"M3 3v5h5"}]];nt(e,at({name:"rotate-ccw"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function us(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"}],["path",{d:"M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7"}],["path",{d:"M7 3v4a1 1 0 0 0 1 1h7"}]];nt(e,at({name:"save"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function wo(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];nt(e,at({name:"search"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Yc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];nt(e,at({name:"settings"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Qc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];nt(e,at({name:"shield"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Xc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];nt(e,at({name:"sun"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}function Zc(e,t){pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=rt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];nt(e,at({name:"zap"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Oe(),d=ge(l);tt(d,()=>t.children??Me),b(s,l)},$$slots:{default:!0}})),he()}var eu=k('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),tu=k('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <button type="button" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function ru(e,t){pe(t,!0);let r=L(""),n=L("");function s(F){var J;F.preventDefault();const R=a(r).trim();if(!R){c(n,p("login.tokenRequired"),!0);return}Dd(R),c(n,""),(J=t.onLogin)==null||J.call(t,R)}var o=tu(),l=i(o),d=i(l),v=i(d),u=i(v),_=g(v,2),w=i(_),m=g(d,2),I=i(m),M=g(m,2),O=i(M),S=i(O),$=g(O,2),B=g($,2);{var U=F=>{var R=eu(),J=i(R);N(()=>y(J,a(n))),b(F,R)};q(B,F=>{a(n)&&F(U)})}var E=g(B,2),A=i(E);N((F,R,J,ne,xe,Ee,Te)=>{y(u,F),$e(_,"aria-label",R),y(w,J),y(I,ne),y(S,xe),$e($,"placeholder",Ee),y(A,Te)},[()=>p("login.title"),()=>p("app.language"),()=>p("app.languageToggle"),()=>p("login.hint"),()=>p("login.accessToken"),()=>p("login.placeholder"),()=>p("login.login")]),te("click",_,function(...F){Da==null||Da.apply(this,F)}),ia("submit",M,s),Gr($,()=>a(r),F=>c(r,F)),b(e,o),he()}Hr(["click"]);function au(e){if(!Number.isFinite(e)||e<0)return"0s";const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),n=Math.floor(e%3600/60),s=Math.floor(e%60),o=[];return t>0&&o.push(`${t}d`),(r>0||o.length>0)&&o.push(`${r}h`),(n>0||o.length>0)&&o.push(`${n}m`),o.push(`${s}s`),o.join(" ")}var nu=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),su=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),ou=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),iu=k('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),lu=k('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),du=k('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),cu=k('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),uu=k('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),fu=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function vu(e,t){pe(t,!0);let r=L(null),n=L(!0),s=L(""),o=L("");function l(A){return typeof A!="string"||A.length===0?p("common.unknown"):A.replaceAll("_"," ").split(" ").map(F=>F.charAt(0).toUpperCase()+F.slice(1)).join(" ")}function d(A){const F=`channels.names.${A}`,R=p(F);return R===F?l(A):R}const v=Ze(()=>{var A,F,R,J,ne;return[{label:p("overview.version"),value:((A=a(r))==null?void 0:A.version)??p("common.na")},{label:p("overview.uptime"),value:typeof((F=a(r))==null?void 0:F.uptime_seconds)=="number"?au(a(r).uptime_seconds):p("common.na")},{label:p("overview.model"),value:((R=a(r))==null?void 0:R.model)??p("common.na")},{label:p("overview.memoryBackend"),value:((J=a(r))==null?void 0:J.memory_backend)??p("common.na")},{label:p("overview.gatewayPort"),value:(ne=a(r))!=null&&ne.gateway_port?String(a(r).gateway_port):p("common.na")}]}),u=Ze(()=>{var A;return Array.isArray((A=a(r))==null?void 0:A.channels)?a(r).channels:[]});async function _(){try{const A=await kt.getStatus();c(r,A,!0),c(s,""),c(o,new Date().toLocaleTimeString(),!0)}catch(A){c(s,A instanceof Error?A.message:p("overview.loadFailed"),!0)}finally{c(n,!1)}}ar(()=>{let A=!1;const F=async()=>{A||await _()};F();const R=setInterval(F,3e4);return()=>{A=!0,clearInterval(R)}});var w=fu(),m=i(w),I=i(m),M=i(I),O=g(I,2);{var S=A=>{var F=nu(),R=i(F);N(J=>y(R,J),[()=>p("common.updatedAt",{time:a(o)})]),b(A,F)};q(O,A=>{a(o)&&A(S)})}var $=g(m,2);{var B=A=>{var F=su(),R=i(F);N(J=>y(R,J),[()=>p("overview.loading")]),b(A,F)},U=A=>{var F=ou(),R=i(F);N(()=>y(R,a(s))),b(A,F)},E=A=>{var F=uu(),R=ge(F);lt(R,21,()=>a(v),zt,(ae,de)=>{var le=iu(),je=i(le),Y=i(je),ke=g(je,2),We=i(ke);N(()=>{y(Y,a(de).label),y(We,a(de).value)}),b(ae,le)});var J=g(R,2),ne=i(J),xe=i(ne),Ee=g(ne,2);{var Te=ae=>{var de=lu(),le=i(de);N(je=>y(le,je),[()=>p("overview.noChannelsConfigured")]),b(ae,de)},W=ae=>{var de=cu();lt(de,21,()=>a(u),zt,(le,je)=>{var Y=du(),ke=i(Y);N(We=>y(ke,We),[()=>d(a(je))]),b(le,Y)}),b(ae,de)};q(Ee,ae=>{a(u).length===0?ae(Te):ae(W,-1)})}N(ae=>y(xe,ae),[()=>p("overview.configuredChannels")]),b(A,F)};q($,A=>{a(n)?A(B):a(s)?A(U,1):A(E,-1)})}N(A=>y(M,A),[()=>p("overview.title")]),b(e,w),he()}var gu=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),pu=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),hu=k("<option> </option>"),yu=k("<option> </option>"),bu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),_u=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),mu=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),xu=k('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"><span class="rounded-full border border-gray-300/70 px-2 py-1 text-xs dark:border-gray-600/70"> </span></td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),ku=k('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div> <div class="flex items-center justify-between gap-3"><p class="text-sm text-gray-500 dark:text-gray-400"> </p> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div>',1),wu=k('<section class="space-y-6"><div class="flex flex-wrap items-center justify-between gap-3"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></div> <div class="grid gap-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800 lg:grid-cols-[minmax(0,1.3fr)_220px_220px_auto]"><input type="search" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"><option> </option><!></select> <select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500"> </button></div> <!></section>');function Su(e,t){pe(t,!0);const r=20,n=["all","active","pending","empty"];let s=L(it([])),o=L(!0),l=L(!1),d=L(""),v=L(""),u=L(""),_=L("all"),w=L(""),m=L(0),I=L(!1);function M(C){return typeof C!="string"||C.length===0?p("common.unknown"):C.replaceAll("_"," ").split(" ").map(z=>z.charAt(0).toUpperCase()+z.slice(1)).join(" ")}function O(C){const z=`channels.names.${C}`,K=p(z);return K===z?M(C):K}function S(C){const z=`sessions.status.${C}`,K=p(z);return K===z?M(C):K}const $=Ze(()=>[...new Set(a(s).map(C=>C.channel).filter(Boolean))].sort((C,z)=>C.localeCompare(z)));async function B({reset:C=!1,targetPage:z}={}){const K=typeof z=="number"?z:C?0:a(m);c(C?o:l,!0);try{const H=await kt.getSessions({limit:r+1,offset:K*r,channel:a(u)||void 0,status:a(_)==="all"?void 0:a(_),search:a(w).trim()||void 0}),ee=Array.isArray(H)?H:[];c(I,ee.length>r),c(s,a(I)?ee.slice(0,r):ee,!0),c(d,""),c(v,new Date().toLocaleTimeString(),!0),c(m,K,!0)}catch(H){c(d,H instanceof Error?H.message:p("sessions.loadFailed"),!0),C&&c(s,[],!0)}finally{c(o,!1),c(l,!1)}}function U(C){la(`/chat/${encodeURIComponent(C)}`)}function E(){B({reset:!0})}function A(){a(m)!==0&&B({targetPage:a(m)-1})}function F(){a(I)&&B({targetPage:a(m)+1})}ar(()=>{let C=!1;const z=async()=>{C||await B({reset:!0})};z();const K=setInterval(z,15e3);return()=>{C=!0,clearInterval(K)}});var R=wu(),J=i(R),ne=i(J),xe=i(ne),Ee=i(xe),Te=g(xe,2);{var W=C=>{var z=gu(),K=i(z);N(H=>y(K,H),[()=>p("common.loading")]),b(C,z)};q(Te,C=>{a(l)&&!a(o)&&C(W)})}var ae=g(ne,2);{var de=C=>{var z=pu(),K=i(z);N(H=>y(K,H),[()=>p("common.updatedAt",{time:a(v)})]),b(C,z)};q(ae,C=>{a(v)&&C(de)})}var le=g(J,2),je=i(le),Y=g(je,2),ke=i(Y),We=i(ke);ke.value=ke.__value="";var Mt=g(ke);lt(Mt,17,()=>a($),zt,(C,z)=>{var K=hu(),H=i(K),ee={};N(Ae=>{y(H,Ae),ee!==(ee=a(z))&&(K.value=(K.__value=a(z))??"")},[()=>O(a(z))]),b(C,K)});var X=g(Y,2);lt(X,21,()=>n,zt,(C,z)=>{var K=yu(),H=i(K),ee={};N(Ae=>{y(H,Ae),ee!==(ee=a(z))&&(K.value=(K.__value=a(z))??"")},[()=>S(a(z))]),b(C,K)});var we=g(X,2),Ge=i(we),ft=g(le,2);{var jt=C=>{var z=bu(),K=i(z);N(H=>y(K,H),[()=>p("sessions.loading")]),b(C,z)},It=C=>{var z=_u(),K=i(z);N(()=>y(K,a(d))),b(C,z)},St=C=>{var z=mu(),K=i(z);N(H=>y(K,H),[()=>p("sessions.none")]),b(C,z)},Ut=C=>{var z=ku(),K=ge(z),H=i(K),ee=i(H),Ae=i(ee),Ye=i(Ae),De=i(Ye),He=g(Ye),ue=i(He),ze=g(He),Ct=i(ze),ce=g(ze),se=i(ce),ye=g(ce),Fe=i(ye),oe=g(ye),Qe=i(oe),$t=g(ee);lt($t,21,()=>a(s),zt,(G,ie)=>{var fe=xu(),Ue=i(fe),_t=i(Ue),Xe=g(Ue),x=i(Xe),D=g(Xe),V=i(D),Q=g(D),Ve=i(Q),Ne=i(Ve),ve=g(Q),gt=i(ve),st=g(ve),be=i(st);N((_e,dt,Tt)=>{y(_t,a(ie).session_id),y(x,a(ie).sender),y(V,_e),y(Ne,dt),y(gt,a(ie).message_count),y(be,Tt)},[()=>O(a(ie).channel),()=>S(a(ie).status),()=>a(ie).last_message_preview||p("common.empty")]),te("click",fe,()=>U(a(ie).session_id)),b(G,fe)});var vt=g(K,2),ht=i(vt),Et=i(ht),Vt=g(ht,2),Pt=i(Vt),Rt=i(Pt),Dt=g(Pt,2),Kt=i(Dt);N((G,ie,fe,Ue,_t,Xe,x,D,V)=>{y(De,G),y(ue,ie),y(Ct,fe),y(se,Ue),y(Fe,_t),y(Qe,Xe),y(Et,x),Pt.disabled=a(m)===0,y(Rt,D),Dt.disabled=!a(I),y(Kt,V)},[()=>p("sessions.sessionId"),()=>p("sessions.sender"),()=>p("sessions.channel"),()=>p("sessions.statusLabel"),()=>p("sessions.messages"),()=>p("sessions.lastMessage"),()=>p("sessions.pageLabel",{page:a(m)+1}),()=>p("sessions.previousPage"),()=>p("sessions.nextPage")]),te("click",Pt,A),te("click",Dt,F),b(C,z)};q(ft,C=>{a(o)?C(jt):a(d)?C(It,1):a(s).length===0?C(St,2):C(Ut,-1)})}N((C,z,K,H)=>{y(Ee,C),$e(je,"placeholder",z),y(We,K),y(Ge,H)},[()=>p("sessions.title"),()=>p("sessions.searchPlaceholder"),()=>p("sessions.allChannels"),()=>p("sessions.applyFilters")]),te("keydown",je,C=>{C.key==="Enter"&&E()}),Gr(je,()=>a(w),C=>c(w,C)),Sn(Y,()=>a(u),C=>c(u,C)),Sn(X,()=>a(_),C=>c(_,C)),te("click",we,E),b(e,R),he()}Hr(["keydown","click"]);var $u=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Eu=k('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),Au=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Mu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Cu=k('<div class="flex justify-center"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div>'),Pu=k('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),Tu=k('<img class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),Nu=k('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),Ou=k("<div></div>"),Fu=k('<div class="space-y-3"><!> <!></div>'),Iu=k('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),Lu=k('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),Ru=k('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200">DOC</div>'),ju=k('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white"> </button></div>'),Du=k('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),Hu=k('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function zu(e,t){pe(t,!0);const r=10,n=40,s=80,o=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let l=ja(t,"sessionId",3,""),d=L(it([])),v=L(""),u=L(!0),_=L(!1),w=L(!1),m=L(!1),I=L(0),M=L(""),O=L(null),S=L(null),$=L(it([])),B=L(!1),U=0;function E(x){return x!=null&&x.message_id?`id:${x.message_id}`:`fallback:${(x==null?void 0:x.timestamp)??""}:${(x==null?void 0:x.role)??""}:${(x==null?void 0:x.content)??""}`}function A(x){const D=new Set,V=[];for(const Q of x){const Ve=E(Q);D.has(Ve)||(D.add(Ve),V.push(Q))}return V}function F(){la("/sessions")}function R(x){return x==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":x==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function J(x){return((x==null?void 0:x.type)||"").startsWith("image/")}function ne(x){return((x==null?void 0:x.type)||"").startsWith("video/")}function xe(x){if(!Number.isFinite(x)||x<=0)return"0 B";const D=["B","KB","MB","GB"];let V=x,Q=0;for(;V>=1024&&Q<D.length-1;)V/=1024,Q+=1;return`${V.toFixed(Q===0?0:1)} ${D[Q]}`}function Ee(x){return typeof x=="string"&&x.trim().length>0?x:"unknown"}function Te(x){const D=J(x),V=ne(x);return{id:`${x.name}-${x.lastModified}-${Math.random().toString(36).slice(2)}`,file:x,name:x.name,size:x.size,type:Ee(x.type),isImage:D,isVideo:V,previewUrl:D||V?URL.createObjectURL(x):""}}function W(x){x&&typeof x.previewUrl=="string"&&x.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(x.previewUrl)}function ae(){for(const x of a($))W(x);c($,[],!0),a(S)&&(a(S).value="")}function de(x){if(!x||x.length===0||a(_))return;const D=Array.from(x),V=[],Q=Math.max(0,r-a($).length);for(const Ve of D.slice(0,Q))V.push(Te(Ve));c($,[...a($),...V],!0)}function le(x){const D=a($).find(V=>V.id===x);D&&W(D),c($,a($).filter(V=>V.id!==x),!0)}function je(){var x;a(_)||(x=a(S))==null||x.click()}function Y(x){var D;de((D=x.currentTarget)==null?void 0:D.files),a(S)&&(a(S).value="")}function ke(x){x.preventDefault(),!a(_)&&(U+=1,c(B,!0))}function We(x){x.preventDefault(),!a(_)&&x.dataTransfer&&(x.dataTransfer.dropEffect="copy")}function Mt(x){x.preventDefault(),U=Math.max(0,U-1),U===0&&c(B,!1)}function X(x){var D;x.preventDefault(),U=0,c(B,!1),de((D=x.dataTransfer)==null?void 0:D.files)}function we(){!a(O)||a(u)||a(_)||a(w)||!a(m)||a(O).scrollTop<=s&&C()}function Ge(x){const D=(x||"").trim();if(!D)return"";const V=D.toLowerCase();return V.startsWith("data:image/")||V.startsWith("data:video/")||V.startsWith("http://")||V.startsWith("https://")?D:kt.getSessionMediaUrl(D)}function ft(x,D){const V=(D||"").trim().toLowerCase();return x==="VIDEO"||V.startsWith("data:video/")?"video":V.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(Ve=>V.endsWith(Ve))?"video":"image"}function jt(x){if(typeof x!="string"||x.length===0)return[];const D=[];o.lastIndex=0;let V=0,Q;for(;(Q=o.exec(x))!==null;){Q.index>V&&D.push({id:`text-${V}`,kind:"text",value:x.slice(V,Q.index)});const Ve=(Q[1]||"").toUpperCase(),Ne=(Q[2]||Q[3]||"").trim();if(Ne){const ve=ft(Ve,Ne);D.push({id:`${ve}-${Q.index}`,kind:ve,value:Ne})}V=o.lastIndex}return V<x.length&&D.push({id:`text-tail-${V}`,kind:"text",value:x.slice(V)}),D}async function It(){await ws(),a(O)&&(a(O).scrollTop=a(O).scrollHeight)}async function St(x,{appendOlder:D=!1}={}){const V=await kt.getSessionMessages(l(),{limit:n+1,offset:x}),Q=Array.isArray(V)?V:[],Ve=Q.length>n,Ne=Ve?Q.slice(0,n):Q;if(D&&a(O)){const ve=a(O).scrollHeight;c(d,A([...Ne,...a(d)]),!0),c(I,a(d).length,!0),c(m,Ve),await ws(),a(O).scrollTop=a(O).scrollHeight-ve;return}c(d,A(Ne),!0),c(I,a(d).length,!0),c(m,Ve)}async function Ut(){try{await St(0),c(M,""),await It()}catch(x){c(M,x instanceof Error?x.message:p("chat.loadFailed"),!0)}finally{c(u,!1)}}async function C(){if(!(a(w)||!a(m))){c(w,!0);try{await St(a(I),{appendOlder:!0}),c(M,"")}catch(x){c(M,x instanceof Error?x.message:p("chat.loadFailed"),!0)}finally{c(w,!1)}}}async function z(){try{await St(0),c(M,""),await It()}catch(x){c(M,x instanceof Error?x.message:p("chat.loadFailed"),!0)}}async function K(){const x=a(v).trim(),D=a($).map(Q=>Q.file);if(x.length===0&&D.length===0||a(_))return;c(_,!0),c(v,""),c(M,"");const V=D.length>0;V||(c(d,[...a(d),{role:"user",content:x}],!0),await It());try{const Q=V?await kt.sendMessageWithMedia(l(),x,D):await kt.sendMessage(l(),x);V?await z():Q&&typeof Q.reply=="string"&&Q.reply.length>0&&c(d,[...a(d),{role:"assistant",content:Q.reply}],!0),ae()}catch(Q){c(M,Q instanceof Error?Q.message:p("chat.sendFailed"),!0),await z()}finally{c(_,!1),await It()}}function H(x){x.preventDefault(),K()}ar(()=>{let x=!1;return(async()=>{x||(c(u,!0),await Ut())})(),()=>{x=!0}}),Id(()=>{for(const x of a($))W(x)});var ee=Hu(),Ae=i(ee),Ye=i(Ae),De=i(Ye),He=i(De),ue=g(De,2),ze=i(ue),Ct=g(Ye,2),ce=i(Ct),se=g(Ae,2);{var ye=x=>{var D=$u(),V=i(D);N(()=>y(V,a(M))),b(x,D)};q(se,x=>{a(M)&&x(ye)})}var Fe=g(se,2),oe=i(Fe),Qe=i(oe);{var $t=x=>{var D=Eu(),V=i(D);N(Q=>y(V,Q),[()=>p("chat.dropFiles",{count:a($).length,max:r})]),b(x,D)};q(Qe,x=>{a(B)&&x($t)})}var vt=g(Qe,2);{var ht=x=>{var D=Au(),V=i(D);N(Q=>y(V,Q),[()=>p("chat.loading")]),b(x,D)},Et=x=>{var D=Mu(),V=i(D);N(Q=>y(V,Q),[()=>p("chat.empty")]),b(x,D)},Vt=x=>{var D=Fu(),V=i(D);{var Q=Ne=>{var ve=Cu(),gt=i(ve),st=i(gt);N(be=>{gt.disabled=a(w),y(st,be)},[()=>a(w)?p("chat.loadingMore"):p("chat.loadMore")]),te("click",gt,C),b(Ne,ve)};q(V,Ne=>{a(m)&&Ne(Q)})}var Ve=g(V,2);lt(Ve,19,()=>a(d),(Ne,ve)=>Ne.message_id??`${Ne.timestamp??"local"}-${ve}`,(Ne,ve)=>{var gt=Ou();lt(gt,21,()=>jt(a(ve).content),st=>st.id,(st,be)=>{var _e=Oe(),dt=ge(_e);{var Tt=qt=>{var Qt=Oe(),pr=ge(Qt);{var Zr=Bt=>{var h=Pu(),f=i(h);N(()=>y(f,a(be).value)),b(Bt,h)},Xt=Ze(()=>a(be).value.trim().length>0);q(pr,Bt=>{a(Xt)&&Bt(Zr)})}b(qt,Qt)},xr=qt=>{var Qt=Tu();N((pr,Zr)=>{$e(Qt,"src",pr),$e(Qt,"alt",Zr)},[()=>Ge(a(be).value),()=>p("chat.attachmentAlt")]),b(qt,Qt)},kr=qt=>{var Qt=Nu();N(pr=>$e(Qt,"src",pr),[()=>Ge(a(be).value)]),b(qt,Qt)};q(dt,qt=>{a(be).kind==="text"?qt(Tt):a(be).kind==="image"?qt(xr,1):a(be).kind==="video"&&qt(kr,2)})}b(st,_e)}),N(st=>bt(gt,1,st),[()=>Ks(R(a(ve).role))]),b(Ne,gt)}),b(x,D)};q(vt,x=>{a(u)?x(ht):a(d).length===0?x(Et,1):x(Vt,-1)})}Ms(oe,x=>c(O,x),()=>a(O));var Pt=g(oe,2),Rt=i(Pt);Ms(Rt,x=>c(S,x),()=>a(S));var Dt=g(Rt,2);{var Kt=x=>{var D=Du(),V=i(D),Q=i(V),Ve=g(V,2);lt(Ve,21,()=>a($),Ne=>Ne.id,(Ne,ve)=>{var gt=ju(),st=i(gt);{var be=Xt=>{var Bt=Iu();N(()=>{$e(Bt,"src",a(ve).previewUrl),$e(Bt,"alt",a(ve).name)}),b(Xt,Bt)},_e=Xt=>{var Bt=Lu();Bt.muted=!0,N(()=>$e(Bt,"src",a(ve).previewUrl)),b(Xt,Bt)},dt=Xt=>{var Bt=Ru();b(Xt,Bt)};q(st,Xt=>{a(ve).isImage?Xt(be):a(ve).isVideo?Xt(_e,1):Xt(dt,-1)})}var Tt=g(st,2),xr=i(Tt),kr=i(xr),qt=g(xr,2),Qt=i(qt),pr=g(Tt,2),Zr=i(pr);N((Xt,Bt)=>{y(kr,a(ve).name),y(Qt,`${a(ve).type??""} · ${Xt??""}`),y(Zr,Bt)},[()=>xe(a(ve).size),()=>p("chat.removeAttachment")]),te("click",pr,()=>le(a(ve).id)),b(Ne,gt)}),N(Ne=>y(Q,Ne),[()=>p("chat.attachments",{count:a($).length,max:r})]),b(x,D)};q(Dt,x=>{a($).length>0&&x(Kt)})}var G=g(Dt,2),ie=i(G),fe=g(ie,2),Ue=i(fe);Jc(Ue,{size:16});var _t=g(fe,2),Xe=i(_t);N((x,D,V,Q,Ve,Ne,ve,gt)=>{y(He,x),y(ze,`${D??""}: ${l()??""}`),y(ce,V),$e(Fe,"aria-label",Q),bt(oe,1,`flex-1 overflow-y-auto p-4 ${a(B)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),$e(ie,"placeholder",Ve),$e(fe,"title",Ne),fe.disabled=a(_)||a($).length>=r,_t.disabled=ve,y(Xe,gt)},[()=>p("chat.title"),()=>p("chat.session"),()=>p("chat.back"),()=>p("chat.messagesRegion"),()=>p("chat.inputPlaceholder"),()=>p("chat.attachFiles"),()=>a(_)||!a(v).trim()&&a($).length===0,()=>a(_)?p("chat.sending"):p("chat.send")]),te("click",Ct,F),ia("dragenter",Fe,ke),ia("dragover",Fe,We),ia("dragleave",Fe,Mt),ia("drop",Fe,X),ia("scroll",oe,we),ia("submit",Pt,H),te("change",Rt,Y),Gr(ie,()=>a(v),x=>c(v,x)),te("click",fe,je),b(e,ee),he()}Hr(["click","change"]);var Uu=k('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Vu=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Ku=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),qu=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Bu=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),Wu=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Gu=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Ju(e,t){pe(t,!0);let r=L(it([])),n=L(!0),s=L(""),o=L("");function l(E){return typeof E!="string"||E.length===0?p("common.unknown"):E.replaceAll("_"," ").split(" ").map(A=>A.charAt(0).toUpperCase()+A.slice(1)).join(" ")}function d(E){const A=`channels.names.${E}`,F=p(A);return F===A?l(E):F}async function v(){try{const E=await kt.getChannelsStatus();c(r,Array.isArray(E==null?void 0:E.channels)?E.channels:[],!0),c(s,""),c(o,new Date().toLocaleTimeString(),!0)}catch(E){c(s,E instanceof Error?E.message:p("channels.loadFailed"),!0)}finally{c(n,!1)}}ar(()=>{let E=!1;const A=async()=>{E||await v()};A();const F=setInterval(A,3e4);return()=>{E=!0,clearInterval(F)}});var u=Gu(),_=i(u),w=i(_),m=i(w),I=g(w,2);{var M=E=>{var A=Uu(),F=i(A);N(R=>y(F,R),[()=>p("common.updatedAt",{time:a(o)})]),b(E,A)};q(I,E=>{a(o)&&E(M)})}var O=g(_,2);{var S=E=>{var A=Vu(),F=i(A);N(R=>y(F,R),[()=>p("channels.loading")]),b(E,A)},$=E=>{var A=Ku(),F=i(A);N(()=>y(F,a(s))),b(E,A)},B=E=>{var A=qu(),F=i(A);N(R=>y(F,R),[()=>p("channels.noChannels")]),b(E,A)},U=E=>{var A=Wu();lt(A,21,()=>a(r),zt,(F,R)=>{var J=Bu(),ne=i(J),xe=i(ne),Ee=i(xe),Te=g(xe,2),W=i(Te),ae=g(ne,2),de=i(ae),le=g(ae,2),je=i(le);N((Y,ke,We,Mt,X,we)=>{y(Ee,Y),bt(Te,1,`rounded-full px-2 py-1 text-xs font-medium ${a(R).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(W,ke),y(de,`${We??""}: ${Mt??""}`),y(je,`${X??""}: ${we??""}`)},[()=>d(a(R).name),()=>a(R).enabled?p("common.enabled"):p("common.disabled"),()=>p("channels.type"),()=>d(a(R).type),()=>p("channels.status"),()=>d(a(R).status)]),b(F,J)}),b(E,A)};q(O,E=>{a(n)?E(S):a(s)?E($,1):a(r).length===0?E(B,2):E(U,-1)})}N(E=>y(m,E),[()=>p("channels.title")]),b(e,u),he()}const Yu=e=>e;function Qu(e){const t=e-1;return t*t*t+1}function So(e,{delay:t=0,duration:r=400,easing:n=Yu}={}){const s=+getComputedStyle(e).opacity;return{delay:t,duration:r,easing:n,css:o=>`opacity: ${o*s}`}}function $o(e,{delay:t=0,duration:r=400,easing:n=Qu,axis:s="y"}={}){const o=getComputedStyle(e),l=+o.opacity,d=s==="y"?"height":"width",v=parseFloat(o[d]),u=s==="y"?["top","bottom"]:["left","right"],_=u.map($=>`${$[0].toUpperCase()}${$.slice(1)}`),w=parseFloat(o[`padding${_[0]}`]),m=parseFloat(o[`padding${_[1]}`]),I=parseFloat(o[`margin${_[0]}`]),M=parseFloat(o[`margin${_[1]}`]),O=parseFloat(o[`border${_[0]}Width`]),S=parseFloat(o[`border${_[1]}Width`]);return{delay:t,duration:r,easing:n,css:$=>`overflow: hidden;opacity: ${Math.min($*20,1)*l};${d}: ${$*v}px;padding-${u[0]}: ${$*w}px;padding-${u[1]}: ${$*m}px;margin-${u[0]}: ${$*I}px;margin-${u[1]}: ${$*M}px;border-${u[0]}-width: ${$*O}px;border-${u[1]}-width: ${$*S}px;min-${d}: 0`}}var Xu=k('<button type="button"><span class="config-toggle__thumb svelte-18svoa7"></span></button>'),Zu=k("<option> </option>"),ef=k('<select class="config-input svelte-18svoa7"></select>'),tf=k('<input class="config-input svelte-18svoa7" type="number"/>'),rf=k('<label class="tag-chip svelte-18svoa7"><input type="text" class="svelte-18svoa7"/> <button type="button" class="svelte-18svoa7">×</button></label>'),af=k('<div class="tag-editor svelte-18svoa7"><div class="tag-list svelte-18svoa7"></div> <button type="button" class="secondary-action svelte-18svoa7">Add tag</button></div>'),nf=k('<textarea class="config-editor svelte-18svoa7" rows="6"></textarea>'),sf=k('<button type="button" class="icon-action svelte-18svoa7" aria-label="Toggle visibility"><!></button>'),of=k('<div class="field-input-row svelte-18svoa7"><input class="config-input svelte-18svoa7"/> <!></div>'),lf=k('<span class="config-badge svelte-18svoa7">Modified</span>'),df=k('<span class="config-badge config-badge--muted svelte-18svoa7">Unsaved</span>'),cf=k('<button type="button" class="ghost-action svelte-18svoa7"><!> Reset</button>'),uf=k('<p class="config-field__description svelte-18svoa7"> </p>'),ff=k("<span> </span>"),vf=k('<article><div class="config-field__meta svelte-18svoa7"><div class="config-field__heading svelte-18svoa7"><div><div class="config-field__title-row svelte-18svoa7"><h4> </h4> <!> <!></div> <p class="config-field__path svelte-18svoa7"> </p></div> <!></div> <!> <div class="config-field__hint-row svelte-18svoa7"><span> </span> <!></div></div> <div class="config-field__control"><!></div></article>'),gf=k('<span class="config-badge svelte-18svoa7">Modified</span>'),pf=k('<span class="config-badge config-badge--muted svelte-18svoa7">Unsaved</span>'),hf=k('<p class="object-card__description svelte-18svoa7"> </p>'),yf=k('<p class="empty-state svelte-18svoa7">No matching fields.</p>'),bf=k('<div class="object-card__grid svelte-18svoa7"></div>'),_f=k('<div class="object-card__body svelte-18svoa7"><!></div>'),mf=k('<section><button type="button" class="object-card__header svelte-18svoa7"><div><div class="object-card__title-row svelte-18svoa7"><h4> </h4> <!> <!></div> <p class="object-card__path svelte-18svoa7"> </p> <!></div> <!></button> <!></section>'),xf=k('<p class="loading-state svelte-18svoa7">Loading config...</p>'),kf=k('<p class="error-banner svelte-18svoa7"> </p>'),wf=k('<p class="inline-error svelte-18svoa7"> </p>'),Sf=k('<p class="inline-error svelte-18svoa7"> </p>'),$f=k('<article class="file-card svelte-18svoa7"><div class="file-card__header svelte-18svoa7"><div><div class="file-card__title-row svelte-18svoa7"><h4> </h4> <span class="config-badge config-badge--muted svelte-18svoa7"> </span></div> <p class="svelte-18svoa7"> </p></div> <button type="button" class="primary-action svelte-18svoa7"><!> </button></div> <textarea class="config-editor svelte-18svoa7" rows="12"></textarea> <!></article>'),Ef=k('<div class="advanced-grid svelte-18svoa7"><section class="advanced-card svelte-18svoa7"><div class="advanced-card__header svelte-18svoa7"><div><div class="advanced-card__title svelte-18svoa7"><!> <h3 class="svelte-18svoa7">Merged JSON</h3></div> <p class="svelte-18svoa7">Direct editor for the merged runtime config payload.</p></div> <div class="advanced-card__actions svelte-18svoa7"><button type="button" class="secondary-action svelte-18svoa7"><!> Reset</button> <button type="button" class="primary-action svelte-18svoa7"><!> </button></div></div> <textarea class="config-editor config-editor--full svelte-18svoa7" rows="24"></textarea> <!></section> <section class="advanced-card svelte-18svoa7"><div class="advanced-card__header svelte-18svoa7"><div><div class="advanced-card__title svelte-18svoa7"><!> <h3 class="svelte-18svoa7">Config Files</h3></div> <p class="svelte-18svoa7">`config.toml` and `config.d/*.toml` are editable independently.</p></div></div> <div class="file-list svelte-18svoa7"></div></section></div>'),Af=k('<span class="pill__dot svelte-18svoa7"></span>'),Mf=k('<button type="button"><span> </span> <!></button>'),Cf=k('<p class="empty-state svelte-18svoa7">No matching config items.</p>'),Pf=k('<span class="config-badge svelte-18svoa7">Modified</span>'),Tf=k('<span class="config-badge config-badge--muted svelte-18svoa7">Unsaved</span>'),Nf=k('<div class="group-card__grid svelte-18svoa7"></div>'),Of=k('<div class="group-card__body svelte-18svoa7"><!></div>'),Ff=k('<section class="group-card svelte-18svoa7"><button type="button" class="group-card__header svelte-18svoa7"><div class="group-card__title-row svelte-18svoa7"><!> <div><h3 class="svelte-18svoa7"> </h3> <p class="svelte-18svoa7"> </p></div></div> <div class="group-card__summary svelte-18svoa7"><!> <!> <!></div></button> <!></section>'),If=k('<div class="group-list svelte-18svoa7"></div>'),Lf=k('<div class="config-shell svelte-18svoa7"><div class="config-toolbar svelte-18svoa7"><label class="search-box svelte-18svoa7"><!> <input type="search" placeholder="Search by field name or description" class="svelte-18svoa7"/></label> <div class="config-pills svelte-18svoa7"></div></div> <!></div>'),Rf=k('<div class="change-row svelte-18svoa7"><span> </span> <code class="svelte-18svoa7"> </code> <span>→</span> <code class="svelte-18svoa7"> </code></div>'),jf=k('<div class="save-bar svelte-18svoa7"><div class="save-bar__content svelte-18svoa7"><div><p class="svelte-18svoa7"> </p> <span class="svelte-18svoa7">Save writes only changed keys back to the config API.</span></div> <div class="save-bar__actions svelte-18svoa7"><button type="button" class="secondary-action svelte-18svoa7">Discard</button> <button type="button" class="primary-action svelte-18svoa7"><!> </button></div></div> <div class="save-bar__changes svelte-18svoa7"></div></div>'),Df=k("<div> </div>"),Hf=k('<section class="config-page svelte-18svoa7"><div class="config-header svelte-18svoa7"><div><h2 class="svelte-18svoa7"> </h2> <p class="svelte-18svoa7">Schema-driven editor with defaults, search, and config file management.</p></div> <div class="config-header__actions svelte-18svoa7"><label class="mode-switch svelte-18svoa7"><input type="checkbox" class="svelte-18svoa7"/> <span>Advanced mode</span></label> <button type="button" class="secondary-action svelte-18svoa7"><!> Copy JSON</button> <button type="button" class="secondary-action svelte-18svoa7"><!> Reload</button></div></div> <!> <!> <!></section>');function zf(e,t){pe(t,!0);const r=(h,f=Me)=>{const P=Ze(()=>de(a(l),f().path)),j=Ze(()=>a(E).has(f().path));var re=Oe(),Ce=ge(re);{var me=Se=>{var Z=Xu();N(()=>{bt(Z,1,`config-toggle ${a(P)?"is-on":""}`,"svelte-18svoa7"),$e(Z,"aria-label",f().label)}),te("click",Z,()=>Y(f().path,!a(P))),b(Se,Z)},Nt=Se=>{var Z=ef();lt(Z,21,()=>f().enumOptions,zt,(ot,xt)=>{var Le=Zu(),pt=i(Le),Wt={};N(qe=>{y(pt,a(xt).label),Wt!==(Wt=qe)&&(Le.value=(Le.__value=qe)??"")},[()=>String(a(xt).value)]),b(ot,Le)});var Ke;qs(Z),N(()=>{Ke!==(Ke=a(P)??f().defaultValue??"")&&(Z.value=(Z.__value=a(P)??f().defaultValue??"")??"",wn(Z,a(P)??f().defaultValue??""))}),te("change",Z,ot=>{var Le;const xt=(Le=f().enumOptions.find(pt=>String(pt.value)===ot.currentTarget.value))==null?void 0:Le.value;Y(f().path,xt??ot.currentTarget.value)}),b(Se,Z)},ct=Se=>{var Z=tf();N(()=>{gn(Z,a(P)??f().defaultValue??""),$e(Z,"min",f().schema.minimum),$e(Z,"max",f().schema.maximum),$e(Z,"step",f().schema.multipleOf??(f().schema.type==="integer"?1:"any"))}),te("input",Z,Ke=>{const ot=Ke.currentTarget.value;if(ot===""){const Le=Ee(a(l))??{};je(Le,f().path),c(l,Le,!0);return}const xt=f().schema.type==="integer"?parseInt(ot,10):parseFloat(ot);Number.isNaN(xt)||Y(f().path,xt)}),b(Se,Z)},mt=Se=>{var Z=af(),Ke=i(Z);lt(Ke,21,()=>Array.isArray(a(P))?a(P):[],zt,(xt,Le,pt)=>{var Wt=rf(),qe=i(Wt),Ot=g(qe,2);N(()=>gn(qe,a(Le))),te("input",qe,yt=>Ue(f().path,pt,yt.currentTarget.value)),te("click",Ot,()=>_t(f().path,pt)),b(xt,Wt)});var ot=g(Ke,2);te("click",ot,()=>fe(f().path)),b(Se,Z)},Ie=Se=>{var Z=nf();N(Ke=>gn(Z,Ke),[()=>JSON.stringify(a(P)??f().defaultValue??null,null,2)]),ia("blur",Z,Ke=>{try{Y(f().path,JSON.parse(Ke.currentTarget.value))}catch{Ke.currentTarget.value=JSON.stringify(de(a(l),f().path)??f().defaultValue??null,null,2)}}),b(Se,Z)},At=Se=>{var Z=of(),Ke=i(Z),ot=g(Ke,2);{var xt=Le=>{var pt=sf(),Wt=i(pt);{var qe=yt=>{Dc(yt,{size:16})},Ot=yt=>{Hc(yt,{size:16})};q(Wt,yt=>{a(j)?yt(qe):yt(Ot,-1)})}te("click",pt,()=>X(f().path)),b(Le,pt)};q(ot,Le=>{f().sensitive&&Le(xt)})}N(Le=>{$e(Ke,"type",f().sensitive&&!a(j)?"password":"text"),gn(Ke,a(P)??""),$e(Ke,"placeholder",Le)},[()=>f().defaultValue!==void 0?String(f().defaultValue):""]),te("input",Ke,Le=>Y(f().path,Le.currentTarget.value)),b(Se,Z)};q(Ce,Se=>{f().inputKind==="boolean"?Se(me):f().inputKind==="enum"?Se(Nt,1):f().inputKind==="number"?Se(ct,2):f().inputKind==="string-array"?Se(mt,3):f().inputKind==="json"?Se(Ie,4):Se(At,-1)})}b(h,re)},n=(h,f=Me)=>{const P=Ze(()=>de(a(l),f().path));var j=vf(),re=i(j),Ce=i(re),me=i(Ce),Nt=i(me),ct=i(Nt),mt=i(ct),Ie=g(ct,2);{var At=ut=>{var Lt=lf();b(ut,Lt)};q(Ie,ut=>{f().modifiedFromDefault&&ut(At)})}var Se=g(Ie,2);{var Z=ut=>{var Lt=df();b(ut,Lt)};q(Se,ut=>{f().dirtyFromOriginal&&ut(Z)})}var Ke=g(Nt,2),ot=i(Ke),xt=g(me,2);{var Le=ut=>{var Lt=cf(),yr=i(Lt);ko(yr,{size:14}),te("click",Lt,()=>ke(f().path,f().defaultValue)),b(ut,Lt)};q(xt,ut=>{f().defaultValue!==void 0&&ut(Le)})}var pt=g(Ce,2);{var Wt=ut=>{var Lt=uf(),yr=i(Lt);N(()=>y(yr,f().description)),b(ut,Lt)};q(pt,ut=>{f().description&&ut(Wt)})}var qe=g(pt,2),Ot=i(qe),yt=i(Ot),or=g(Ot,2);{var wr=ut=>{var Lt=ff(),yr=i(Lt);N(ma=>y(yr,`Default: ${ma??""}`),[()=>It(f().defaultValue)]),b(ut,Lt)};q(or,ut=>{f().defaultValue!==void 0&&ut(wr)})}var hr=g(re,2),Sr=i(hr);r(Sr,f),N(ut=>{bt(j,1,`config-field ${f().modifiedFromDefault?"is-modified":""} ${f().dirtyFromOriginal?"is-dirty":""}`,"svelte-18svoa7"),y(mt,f().label),y(ot,f().path),y(yt,`Current: ${ut??""}`)},[()=>St(a(P))]),b(h,j)},s=(h,f=Me)=>{var P=mf(),j=i(P),re=i(j),Ce=i(re),me=i(Ce),Nt=i(me),ct=g(me,2);{var mt=qe=>{var Ot=gf();b(qe,Ot)};q(ct,qe=>{f().modifiedFromDefault&&qe(mt)})}var Ie=g(ct,2);{var At=qe=>{var Ot=pf();b(qe,Ot)};q(Ie,qe=>{f().dirtyFromOriginal&&qe(At)})}var Se=g(Ce,2),Z=i(Se),Ke=g(Se,2);{var ot=qe=>{var Ot=hf(),yt=i(Ot);N(()=>y(yt,f().description)),b(qe,Ot)};q(Ke,qe=>{f().description&&qe(ot)})}var xt=g(re,2);{let qe=Ze(()=>`transform: rotate(${Qe(f())?180:0}deg); transition: transform 0.18s ease;`);xo(xt,{size:18,get style(){return a(qe)}})}var Le=g(j,2);{var pt=qe=>{var Ot=_f(),yt=i(Ot);{var or=hr=>{var Sr=yf();b(hr,Sr)},wr=hr=>{var Sr=bf();lt(Sr,21,()=>f().visibleChildren,ut=>ut.id,(ut,Lt)=>{var yr=Oe(),ma=ge(yr);{var sn=zr=>{s(zr,()=>a(Lt))},on=zr=>{n(zr,()=>a(Lt))};q(ma,zr=>{a(Lt).inputKind==="object"?zr(sn):zr(on,-1)})}b(ut,yr)}),b(hr,Sr)};q(yt,hr=>{f().visibleChildren.length===0?hr(or):hr(wr,-1)})}In(3,Ot,()=>$o,()=>({duration:180})),b(qe,Ot)},Wt=Ze(()=>Qe(f()));q(Le,qe=>{a(Wt)&&qe(pt)})}N(qe=>{bt(P,1,`object-card ${f().modifiedFromDefault||f().dirtyFromOriginal?"is-emphasized":""}`,"svelte-18svoa7"),$e(j,"aria-expanded",qe),y(Nt,f().label),y(Z,f().path)},[()=>Qe(f())]),te("click",j,()=>Ge(f().path)),b(h,P)},o=Object.freeze({});let l=L(it({})),d=L(it({})),v=L(null),u=L(it([])),_=L(!0),w=L(""),m=L(""),I=L("success"),M=L(!1),O=L(!1),S=L(""),$=L("provider"),B=L(it(new Set)),U=L(it(new Set)),E=L(it(new Set)),A=L(""),F=L(""),R=L(!1),J=L(it({})),ne=L(it(o));const xe={provider:Zc,gateway:Kc,channels:Wc,agent:Cc,memory:Pc,security:Qc,heartbeat:qc,reliability:Ns,scheduler:Ic,sessions_spawn:Vc,observability:Nc,web_search:wo,cost:jc,runtime:Yc,tunnel:Tc,identity:Mc};function Ee(h){if(h!==void 0)return JSON.parse(JSON.stringify(h))}function Te(h){return h!==null&&typeof h=="object"&&!Array.isArray(h)}function W(h,f){return JSON.stringify(h)===JSON.stringify(f)}function ae(h,f){return Object.prototype.hasOwnProperty.call(h??{},f)}function de(h,f){if(!f)return h;const P=f.split(".");let j=h;for(const re of P)if(!Te(j)&&!Array.isArray(j)||(j=j==null?void 0:j[re],j===void 0))return;return j}function le(h,f,P){const j=f.split(".");let re=h;for(let Ce=0;Ce<j.length-1;Ce+=1){const me=j[Ce];Te(re[me])||(re[me]={}),re=re[me]}re[j[j.length-1]]=P}function je(h,f){const P=f.split(".");let j=h;for(let re=0;re<P.length-1;re+=1)if(j=j==null?void 0:j[P[re]],!Te(j))return;j&&delete j[P[P.length-1]]}function Y(h,f){const P=Ee(a(l))??{};le(P,h,f),c(l,P,!0)}function ke(h,f){f!==void 0&&Y(h,Ee(f))}function We(){c(l,Ee(a(d))??{},!0),c(R,!1),c(F,"")}function Mt(h,f){const P=new Set(h);return P.has(f)?P.delete(f):P.add(f),P}function X(h){c(E,Mt(a(E),h),!0)}function we(h){c(B,Mt(a(B),h),!0)}function Ge(h){c(U,Mt(a(U),h),!0)}function ft(h){if(c($,h,!0),!a(B).has(h)){const f=new Set(a(B));f.add(h),c(B,f,!0)}Ei(h)}function jt(h){const f=String(h).toLowerCase();return["key","token","secret","password","auth","credential","private"].some(P=>f.includes(P))}function It(h){return h===void 0?"No default":typeof h=="string"?h.length>0?h:"(empty)":JSON.stringify(h)}function St(h){return h===void 0?"(unset)":h===null?"null":typeof h=="string"?h.length>0?h:"(empty)":JSON.stringify(h)}function Ut(h,...f){return h?f.filter(j=>typeof j=="string"&&j.trim().length>0).join(" ").toLowerCase().includes(h):!0}function C(h,f){if(!f.startsWith("#/"))return null;const P=f.slice(2).split("/").map(re=>re.replaceAll("~1","/").replaceAll("~0","~"));let j=h;for(const re of P)if(j=j==null?void 0:j[re],j===void 0)return null;return j}function z(h,f){const P={...h,...f};return(h!=null&&h.properties||f!=null&&f.properties)&&(P.properties={...(h==null?void 0:h.properties)??{},...(f==null?void 0:f.properties)??{}}),(h!=null&&h.required||f!=null&&f.required)&&(P.required=Array.from(new Set([...(h==null?void 0:h.required)??[],...(f==null?void 0:f.required)??[]]))),(f==null?void 0:f.items)!==void 0?P.items=f.items:(h==null?void 0:h.items)!==void 0&&(P.items=h.items),P}function K(h){if(!h||typeof h!="object")return{};let f=h;if(f.$ref){const P=C(a(v),f.$ref);P&&(f=z(P,{...f,$ref:void 0}))}if(Array.isArray(f.allOf)&&f.allOf.length>0){let P={...f,allOf:void 0};for(const j of f.allOf)P=z(P,K(j));f=P}return f}function H(h){const f=K(h);return[...f.oneOf??[],...f.anyOf??[]].map(j=>K(j)).filter(j=>!(j.const===null||j.type==="null"||Array.isArray(j.type)&&j.type.length===1&&j.type[0]==="null"))}function ee(h,f){const P=K(h);if(Array.isArray(P.type)){const re=P.type.filter(Ce=>Ce!=="null");if(re.length===1)return re[0]}if(P.type)return P.type;if(P.properties||f&&Te(f))return"object";if(P.items||Array.isArray(f))return"array";const j=H(P);return j.length===1?ee(j[0],f):typeof f=="boolean"?"boolean":typeof f=="number"?Number.isInteger(f)?"integer":"number":typeof f=="string"?"string":null}function Ae(h){const f=K(h);if(Array.isArray(f.enum)&&f.enum.length>0)return f.enum.map(j=>({label:j===null?"(null)":String(j),value:j}));const P=[...f.oneOf??[],...f.anyOf??[]].map(j=>K(j)).filter(j=>j.const!==void 0);return P.length>0?P.map(j=>({label:j.title??(j.const===null?"(null)":String(j.const)),value:j.const})):[]}function Ye(h){return typeof h=="boolean"?{type:"boolean"}:typeof h=="number"?{type:Number.isInteger(h)?"integer":"number"}:typeof h=="string"?{type:"string"}:Array.isArray(h)?h.every(f=>typeof f=="string")?{type:"array",items:{type:"string"},default:[]}:{type:"array"}:Te(h)?{type:"object",properties:Object.fromEntries(Object.entries(h).map(([f,P])=>[f,Ye(P)]))}:{}}function De(h,f){const P=K(h);if(Ae(P).length>0)return"enum";const re=ee(P,f);if(re==="boolean")return"boolean";if(re==="number"||re==="integer")return"number";if(re==="string")return"string";if(re==="object")return"object";if(re==="array")return ee(P.items,Array.isArray(f)?f[0]:void 0)==="string"?"string-array":"json";const Ce=H(P);return Ce.length===2&&Ce.some(me=>ee(me)==="boolean")&&Ce.some(me=>ee(me)==="string")?"enum":"json"}function He(h,f,P,j,re){const Ce=K(f),me=Ce.properties??{},Nt=Object.keys(me),ct=Te(P)?Object.keys(P):[];return[...Nt,...ct.filter(Ie=>!Nt.includes(Ie))].map(Ie=>{const At=h?`${h}.${Ie}`:Ie,Se=me[Ie]??(Ce.additionalProperties&&Ce.additionalProperties!==!0?Ce.additionalProperties:Ye(P==null?void 0:P[Ie]));return ue(At,Ie,Se,P==null?void 0:P[Ie],j+1,re)})}function ue(h,f,P,j,re,Ce){const me=K(P&&Object.keys(P).length>0?P:Ye(j)),Nt=me.title??Cs(f),ct=me.description??"",mt=ae(me,"default")?Ee(me.default):void 0,Ie=De(me,j),At=!W(j,de(a(d),h)),Se=mt!==void 0&&!W(j,mt),Z=Ut(Ce,f,Nt,ct,h);if(Ie==="object"){const ot=He(h,me,j,re,Ce),xt=ot.filter(pt=>pt.visible),Le=Z||xt.some(pt=>pt.subtreeMatches);return{id:h,path:h,key:f,label:Nt,description:ct,defaultValue:mt,dirtyFromOriginal:At,modifiedFromDefault:Se,inputKind:Ie,depth:re,children:ot,visibleChildren:xt,visible:Ce?Le:!0,matchesSelf:Z,subtreeMatches:Le,sensitive:!1}}const Ke=Ce?Z:!0;return{id:h,path:h,key:f,label:Nt,description:ct,defaultValue:mt,currentValue:j,dirtyFromOriginal:At,modifiedFromDefault:Se,inputKind:Ie,depth:re,visible:Ke,matchesSelf:Z,subtreeMatches:Ke,enumOptions:Ae(me),schema:me,sensitive:jt(f)}}function ze(h){var re;const f=h.trim().toLowerCase(),P=mn(a(l)),j=((re=a(v))==null?void 0:re.properties)??{};return P.map(Ce=>{const me=Ce.groupKey,Nt=j[me]??Ye(a(l)[me]),ct=ue(me,me,Nt,a(l)[me],0,f),mt=qn[me];return{...Ce,label:(mt==null?void 0:mt.label)??Ce.label,defaultOpen:(mt==null?void 0:mt.defaultOpen)??!1,icon:xe[me],node:ct}}).filter(Ce=>Ce.node.visible)}const Ct=Ze(()=>ze(a(S))),ce=Ze(()=>a(S).trim().length>0),se=Ze(()=>JSON.stringify(a(l)??{},null,2)),ye=Ze(()=>!W(a(l),a(d))),Fe=Ze(()=>a(Ct).filter(h=>h.node.visible));function oe(h){return a(ce)?h.node.subtreeMatches:a(B).has(h.groupKey)}function Qe(h){return a(ce)?h.subtreeMatches:a(U).has(h.path)}function $t(){const h=[];function f(P,j,re=""){const Ce=Te(P),me=Te(j);if(Ce&&me){const Nt=Array.from(new Set([...Object.keys(P),...Object.keys(j)]));for(const ct of Nt){const mt=re?`${re}.${ct}`:ct;f(P[ct],j[ct],mt)}return}W(P,j)||h.push({path:re,label:Cs(re.split(".").at(-1)??re),previous:j,current:P})}return f(a(l),a(d)),h}const vt=Ze($t);function ht(h){return a(vt).some(f=>f.path===h||f.path.startsWith(`${h}.`))}function Et(){a(R)||(c(A,a(se),!0),c(F,""))}function Vt(){var f;const h=new Set;for(const P of mn(a(l)))(f=qn[P.groupKey])!=null&&f.defaultOpen&&h.add(P.groupKey);c(B,h,!0)}function Pt(h){c(J,Object.fromEntries(h.map(f=>[f.path,f.content])),!0),c(ne,o,!0)}async function Rt(){c(_,!0),c(w,"");try{const[h,f]=await Promise.all([kt.getConfigSchema(),kt.getConfigFiles()]);await Ai({force:!0}),c(l,Ee(Zt.data)??{},!0),c(d,Ee(Zt.data)??{},!0),c(v,h??{},!0),c(u,Array.isArray(f)?f:[],!0),Pt(a(u)),Vt(),c(U,new Set,!0),c(R,!1),Et()}catch(h){c(w,h instanceof Error?h.message:p("config.loadFailed"),!0)}finally{c(_,!1)}}async function Dt(){if(!(!a(ye)||a(M))){c(M,!0),c(m,""),c(I,"success");try{const h={};for(const P of a(vt))le(h,P.path,P.current);const f=await kt.saveConfig(h);bo(Ee(a(l))??{}),c(d,Ee(a(l))??{},!0),c(R,!1),Et(),f!=null&&f.restart_required?c(m,p("config.saveRestartRequired"),!0):c(m,p("config.saveSuccess"),!0),setTimeout(()=>{c(m,"")},5e3)}catch(h){c(I,"error"),c(m,p("config.saveFailed",{message:h instanceof Error?h.message:String(h)}),!0)}finally{c(M,!1)}}}async function Kt(){if(!a(M)){c(M,!0),c(F,""),c(m,""),c(I,"success");try{const h=JSON.parse(a(A)),f=await kt.saveConfig(h);c(l,Ee(h)??{},!0),c(d,Ee(h)??{},!0),bo(Ee(h)??{}),c(R,!1),Et(),f!=null&&f.restart_required?c(m,p("config.saveRestartRequired"),!0):c(m,p("config.saveSuccess"),!0),setTimeout(()=>{c(m,"")},5e3)}catch(h){const f=h instanceof Error?h.message:String(h);c(F,f,!0),c(I,"error"),c(m,p("config.saveFailed",{message:f}),!0)}finally{c(M,!1)}}}async function G(h){const f=a(J)[h.path]??"";c(ne,{...a(ne),[h.path]:{saving:!0,error:""}},!0);try{const P=await kt.saveConfigFile(h.filename,f);await Rt(),c(I,"success"),c(m,P!=null&&P.restart_required?p("config.saveRestartRequired"):p("config.saveSuccess"),!0),setTimeout(()=>{c(m,"")},5e3)}catch(P){c(ne,{...a(ne),[h.path]:{saving:!1,error:P instanceof Error?P.message:String(P)}},!0);return}c(ne,{...a(ne),[h.path]:{saving:!1,error:""}},!0)}async function ie(h){if(!(typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(h)}catch{}}function fe(h){const f=de(a(l),h),P=Array.isArray(f)?[...f,""]:[""];Y(h,P)}function Ue(h,f,P){const j=de(a(l),h),re=Array.isArray(j)?[...j]:[];re[f]=P,Y(h,re)}function _t(h,f){const P=de(a(l),h);Array.isArray(P)&&Y(h,P.filter((j,re)=>re!==f))}function Xe(){if(typeof window>"u")return;const h=window.location.hash.replace(/^#/,"");if(!h.startsWith("config-section-"))return;const f=h.replace(/^config-section-/,"");a(Ct).some(P=>P.groupKey===f)&&ft(f)}ar(()=>{Rt()}),ar(()=>{Et()}),ar(()=>{a(_)||a(O)||a(Ct).length===0||queueMicrotask(()=>{Xe()})});var x=Hf(),D=i(x),V=i(D),Q=i(V),Ve=i(Q),Ne=g(V,2),ve=i(Ne),gt=i(ve),st=g(ve,2),be=i(st);Lc(be,{size:14});var _e=g(st,2),dt=i(_e);Ns(dt,{size:14});var Tt=g(D,2);{var xr=h=>{var f=xf();b(h,f)},kr=h=>{var f=kf(),P=i(f);N(()=>y(P,a(w))),b(h,f)},qt=h=>{var f=Ef(),P=i(f),j=i(P),re=i(j),Ce=i(re),me=i(Ce);zc(me,{size:18});var Nt=g(re,2),ct=i(Nt),mt=i(ct);ko(mt,{size:14});var Ie=g(ct,2),At=i(Ie);us(At,{size:14});var Se=g(At),Z=g(j,2),Ke=g(Z,2);{var ot=yt=>{var or=wf(),wr=i(or);N(()=>y(wr,a(F))),b(yt,or)};q(Ke,yt=>{a(F)&&yt(ot)})}var xt=g(P,2),Le=i(xt),pt=i(Le),Wt=i(pt),qe=i(Wt);Uc(qe,{size:18});var Ot=g(Le,2);lt(Ot,21,()=>a(u),yt=>yt.path,(yt,or)=>{const wr=Ze(()=>a(ne)[a(or).path]);var hr=$f(),Sr=i(hr),ut=i(Sr),Lt=i(ut),yr=i(Lt),ma=i(yr),sn=g(yr,2),on=i(sn),zr=g(Lt,2),rs=i(zr),wt=g(ut,2),ir=i(wt);us(ir,{size:14});var ln=g(ir),xa=g(Sr,2),dn=g(xa,2);{var sa=Tr=>{var $r=Sf(),cn=i($r);N(()=>y(cn,a(wr).error)),b(Tr,$r)};q(dn,Tr=>{var $r;($r=a(wr))!=null&&$r.error&&Tr(sa)})}N(()=>{var Tr,$r;y(ma,a(or).path),y(on,a(or).source==="main"?"config.toml":"config.d"),y(rs,a(or).filename),wt.disabled=(Tr=a(wr))==null?void 0:Tr.saving,y(ln,` ${($r=a(wr))!=null&&$r.saving?"Saving...":"Save file"}`),gn(xa,a(J)[a(or).path]??"")}),te("click",wt,()=>G(a(or))),te("input",xa,Tr=>{c(J,{...a(J),[a(or).path]:Tr.currentTarget.value},!0)}),b(yt,hr)}),N(()=>{Ie.disabled=a(M),y(Se,` ${a(M)?"Saving...":"Save JSON"}`)}),te("click",ct,()=>{c(A,a(se),!0),c(R,!1),c(F,"")}),te("click",Ie,Kt),te("input",Z,()=>{c(R,!0),c(F,"")}),Gr(Z,()=>a(A),yt=>c(A,yt)),b(h,f)},Qt=h=>{var f=Lf(),P=i(f),j=i(P),re=i(j);wo(re,{size:16});var Ce=g(re,2),me=g(j,2);lt(me,21,()=>a(Fe),Ie=>Ie.groupKey,(Ie,At)=>{var Se=Mf(),Z=i(Se),Ke=i(Z),ot=g(Z,2);{var xt=pt=>{var Wt=Af();b(pt,Wt)},Le=Ze(()=>ht(a(At).groupKey));q(ot,pt=>{a(Le)&&pt(xt)})}N(()=>{bt(Se,1,`pill ${a($)===a(At).groupKey?"is-active":""}`,"svelte-18svoa7"),y(Ke,a(At).label)}),te("click",Se,()=>ft(a(At).groupKey)),b(Ie,Se)});var Nt=g(P,2);{var ct=Ie=>{var At=Cf();b(Ie,At)},mt=Ie=>{var At=If();lt(At,21,()=>a(Fe),Se=>Se.groupKey,(Se,Z)=>{var Ke=Ff(),ot=i(Ke),xt=i(ot),Le=i(xt);{var pt=wt=>{var ir=Oe(),ln=ge(ir);gd(ln,()=>a(Z).icon,(xa,dn)=>{dn(xa,{size:18})}),b(wt,ir)},Wt=wt=>{Rc(wt,{size:18})};q(Le,wt=>{a(Z).icon?wt(pt):wt(Wt,-1)})}var qe=g(Le,2),Ot=i(qe),yt=i(Ot),or=g(Ot,2),wr=i(or),hr=g(xt,2),Sr=i(hr);{var ut=wt=>{var ir=Pf();b(wt,ir)};q(Sr,wt=>{a(Z).node.modifiedFromDefault&&wt(ut)})}var Lt=g(Sr,2);{var yr=wt=>{var ir=Tf();b(wt,ir)},ma=Ze(()=>ht(a(Z).groupKey));q(Lt,wt=>{a(ma)&&wt(yr)})}var sn=g(Lt,2);{let wt=Ze(()=>`transform: rotate(${oe(a(Z))?180:0}deg); transition: transform 0.18s ease;`);xo(sn,{size:18,get style(){return a(wt)}})}var on=g(ot,2);{var zr=wt=>{var ir=Of(),ln=i(ir);{var xa=sa=>{var Tr=Nf();lt(Tr,21,()=>a(Z).node.visibleChildren,$r=>$r.id,($r,cn)=>{var Ys=Oe(),Pi=ge(Ys);{var Ti=La=>{s(La,()=>a(cn))},Ni=La=>{n(La,()=>a(cn))};q(Pi,La=>{a(cn).inputKind==="object"?La(Ti):La(Ni,-1)})}b($r,Ys)}),b(sa,Tr)},dn=sa=>{n(sa,()=>a(Z).node)};q(ln,sa=>{a(Z).node.inputKind==="object"?sa(xa):sa(dn,-1)})}In(3,ir,()=>$o,()=>({duration:200})),b(wt,ir)},rs=Ze(()=>oe(a(Z)));q(on,wt=>{a(rs)&&wt(zr)})}N((wt,ir)=>{$e(Ke,"id",wt),$e(ot,"aria-expanded",ir),y(yt,a(Z).label),y(wr,a(Z).groupKey)},[()=>Ps(a(Z).groupKey),()=>oe(a(Z))]),te("click",ot,()=>{we(a(Z).groupKey),c($,a(Z).groupKey,!0)}),b(Se,Ke)}),b(Ie,At)};q(Nt,Ie=>{a(Fe).length===0?Ie(ct):Ie(mt,-1)})}Gr(Ce,()=>a(S),Ie=>c(S,Ie)),b(h,f)};q(Tt,h=>{a(_)?h(xr):a(w)?h(kr,1):a(O)?h(qt,2):h(Qt,-1)})}var pr=g(Tt,2);{var Zr=h=>{var f=jf(),P=i(f),j=i(P),re=i(j),Ce=i(re),me=g(j,2),Nt=i(me),ct=g(Nt,2),mt=i(ct);us(mt,{size:14});var Ie=g(mt),At=g(P,2);lt(At,21,()=>a(vt),Se=>Se.path,(Se,Z)=>{var Ke=Rf(),ot=i(Ke),xt=i(ot),Le=g(ot,2),pt=i(Le),Wt=g(Le,4),qe=i(Wt);N((Ot,yt)=>{y(xt,a(Z).path),y(pt,Ot),y(qe,yt)},[()=>St(a(Z).previous),()=>St(a(Z).current)]),b(Se,Ke)}),N(()=>{y(Ce,`${a(vt).length??""} unsaved change(s)`),ct.disabled=a(M),y(Ie,` ${a(M)?"Saving...":"Save config"}`)}),te("click",Nt,We),te("click",ct,Dt),In(3,f,()=>So),b(h,f)};q(pr,h=>{!a(O)&&a(ye)&&!a(_)&&h(Zr)})}var Xt=g(pr,2);{var Bt=h=>{var f=Df(),P=i(f);N(()=>{bt(f,1,`toast ${a(I)==="error"?"is-error":""}`,"svelte-18svoa7"),y(P,a(m))}),In(3,f,()=>So),b(h,f)};q(Xt,h=>{a(m)&&h(Bt)})}N(h=>y(Ve,h),[()=>p("config.title")]),Pd(gt,()=>a(O),h=>c(O,h)),te("click",st,()=>ie(a(se))),te("click",_e,()=>Rt()),b(e,x),he()}Hr(["click","change","input"]);var Uf=k('<p class="text-gray-400 dark:text-gray-500"> </p>'),Vf=k('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),Kf=k('<ol class="space-y-1"></ol>'),qf=k('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function Bf(e,t){pe(t,!0);const r=1e3,n=500,s=1e4;let o=L(it([])),l=L(!1),d=L("disconnected"),v=L(null),u=null,_=null,w=0,m=!0;const I=Ze(()=>a(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":a(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),M=Ze(()=>a(d)==="connected"?p("logs.connected"):a(d)==="reconnecting"?p("logs.reconnecting"):p("logs.disconnected"));function O(){const X=Kn?new URL(Kn,window.location.href):new URL(window.location.href);return X.protocol=X.protocol==="https:"?"wss:":"ws:",X.pathname="/api/logs/stream",X.search="",X.hash="",X.toString()}function S(X){if(typeof X!="string"||X.length===0)return;const we=X.split(/\r?\n/).filter(ft=>ft.length>0);if(we.length===0)return;const Ge=[...a(o),...we];c(o,Ge.length>r?Ge.slice(Ge.length-r):Ge,!0)}function $(){_!==null&&(clearTimeout(_),_=null)}function B(){u&&(u.onopen=null,u.onmessage=null,u.onerror=null,u.onclose=null,u.close(),u=null)}function U(){if(!m){c(d,"disconnected");return}c(d,"reconnecting");const X=Math.min(n*2**w,s);w+=1,$(),_=setTimeout(()=>{_=null,E()},X)}function E(){$(),c(d,"reconnecting"),B();let X;try{X=new WebSocket(O())}catch{U();return}u=X,X.onopen=()=>{w=0,c(d,"connected")},X.onmessage=we=>{a(l)||S(we.data)},X.onerror=()=>{(X.readyState===WebSocket.OPEN||X.readyState===WebSocket.CONNECTING)&&X.close()},X.onclose=()=>{u=null,U()}}function A(){c(l,!a(l))}function F(){c(o,[],!0)}ar(()=>(m=!0,E(),()=>{m=!1,$(),B(),c(d,"disconnected")})),ar(()=>{a(o).length,a(l),!(a(l)||!a(v))&&queueMicrotask(()=>{a(v)&&(a(v).scrollTop=a(v).scrollHeight)})});var R=qf(),J=i(R),ne=i(J),xe=i(ne),Ee=g(ne,2),Te=i(Ee),W=i(Te),ae=g(Te,2),de=i(ae),le=g(ae,2),je=i(le),Y=g(J,2),ke=i(Y);{var We=X=>{var we=Uf(),Ge=i(we);N(ft=>y(Ge,ft),[()=>p("logs.waiting")]),b(X,we)},Mt=X=>{var we=Kf();lt(we,21,()=>a(o),zt,(Ge,ft,jt)=>{var It=Vf(),St=i(It),Ut=i(St),C=g(St,2),z=i(C);N(K=>{y(Ut,K),y(z,a(ft))},[()=>String(jt+1).padStart(4,"0")]),b(Ge,It)}),b(X,we)};q(ke,X=>{a(o).length===0?X(We):X(Mt,-1)})}Ms(Y,X=>c(v,X),()=>a(v)),N((X,we,Ge)=>{y(xe,X),bt(Te,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${a(I)}`),y(W,a(M)),y(de,we),y(je,Ge)},[()=>p("logs.title"),()=>a(l)?p("logs.resume"):p("logs.pause"),()=>p("logs.clear")]),te("click",ae,A),te("click",le,F),b(e,R),he()}Hr(["click"]);var Wf=k("<option> </option>"),Gf=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Jf=k('<div class="space-y-3 rounded-xl border border-sky-500/30 bg-white p-4 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </span> <button type="button" disabled=""><span></span></button> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div></div> <!> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div>'),Yf=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Qf=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Xf=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Zf=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),ev=k("<option> </option>"),tv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),rv=k('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </span> <button type="button" disabled=""><span></span></button> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div></div> <!> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div>'),av=k('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"> </button></div></div>'),nv=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),sv=k('<!> <div class="space-y-3"></div>',1),ov=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <div class="flex items-center gap-3 rounded-xl border border-gray-200 bg-white px-4 py-3 dark:border-gray-700 dark:bg-gray-800"><span class="text-sm font-medium text-gray-700 dark:text-gray-200"> </span> <button type="button"><span></span></button> <span class="text-xs text-gray-500 dark:text-gray-400"> </span></div> <!> <!></section>');function iv(e,t){pe(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call","turn_complete","error"];let n=L(it([])),s=L(!0),o=L(!0),l=L(""),d=L(""),v=L(null),u=L(!1),_=L(!1),w=L(""),m=L(""),I="hook-add",M=L(it(r[0])),O=L(""),S=L(3e4),$=L(!0);function B(){c(M,r[0],!0),c(O,""),c(S,3e4),c($,!0)}function U(H,ee){return`${H}-${ee}`}function E(H){return H.split("_").map(ee=>ee.charAt(0).toUpperCase()+ee.slice(1)).join(" ")}function A(){return a(O).trim()?!Number.isFinite(Number(a(S)))||Number(a(S))<1e3?(c(d,p("hooks.timeoutInvalid"),!0),!1):!0:(c(d,p("hooks.commandRequired"),!0),!1)}async function F(){c(o,!0);try{const H=await kt.getHooks();c(n,Array.isArray(H==null?void 0:H.hooks)?H.hooks:[],!0),c(s,(H==null?void 0:H.enabled)!==!1),c(l,""),c(d,"")}catch(H){c(n,[],!0),c(s,!0),c(l,H instanceof Error?H.message:p("hooks.loadFailed"),!0)}finally{c(o,!1)}}function R(H){c(v,H.id,!0),c(d,""),c(M,H.event,!0),c(O,H.command,!0),c(S,H.timeout_ms,!0),c($,H.enabled,!0)}function J(){c(v,null),c(d,""),B()}async function ne(H){if(A()){c(_,!0),c(d,"");try{await kt.updateHook(H,{event:a(M),command:a(O).trim(),timeout_ms:Number(a(S))}),c(v,null),B(),await F()}catch(ee){c(d,ee instanceof Error?ee.message:p("hooks.saveFailed"),!0)}finally{c(_,!1)}}}async function xe(){if(A()){c(_,!0),c(d,"");try{await kt.createHook({event:a(M),command:a(O).trim(),timeout_ms:Number(a(S))}),c(u,!1),B(),await F()}catch(H){c(d,H instanceof Error?H.message:p("hooks.saveFailed"),!0)}finally{c(_,!1)}}}async function Ee(H){c(w,H,!0),c(d,"");try{await kt.deleteHook(H),a(v)===H&&J(),await F()}catch(ee){c(d,ee instanceof Error?ee.message:p("hooks.deleteFailed"),!0)}finally{c(w,"")}}async function Te(H){c(m,H,!0),c(d,"");try{await kt.toggleHook(H),await F()}catch(ee){c(d,ee instanceof Error?ee.message:p("hooks.toggleFailed"),!0)}finally{c(m,"")}}ar(()=>{F()});var W=ov(),ae=i(W),de=i(ae),le=i(de),je=g(de,2),Y=i(je),ke=g(ae,2),We=i(ke),Mt=i(We),X=g(We,2),we=i(X),Ge=g(X,2),ft=i(Ge),jt=g(ke,2);{var It=H=>{var ee=Jf(),Ae=i(ee),Ye=i(Ae),De=g(Ae,2),He=i(De),ue=i(He),ze=i(ue),Ct=g(ue,2);lt(Ct,21,()=>r,zt,(D,V)=>{var Q=Wf(),Ve=i(Q),Ne={};N(ve=>{y(Ve,ve),Ne!==(Ne=a(V))&&(Q.value=(Q.__value=a(V))??"")},[()=>E(a(V))]),b(D,Q)});var ce=g(He,2),se=i(ce),ye=i(se),Fe=g(se,2),oe=g(ce,2),Qe=i(oe),$t=i(Qe),vt=g(Qe,2),ht=g(oe,2),Et=i(ht),Vt=i(Et),Pt=g(Et,2),Rt=i(Pt),Dt=g(Pt,2),Kt=i(Dt),G=g(De,2);{var ie=D=>{var V=Gf(),Q=i(V);N(()=>y(Q,a(d))),b(D,V)};q(G,D=>{a(d)&&D(ie)})}var fe=g(G,2),Ue=i(fe),_t=i(Ue),Xe=g(Ue,2),x=i(Xe);N((D,V,Q,Ve,Ne,ve,gt,st,be,_e,dt,Tt,xr,kr,qt,Qt)=>{y(Ye,D),$e(ue,"for",V),y(ze,Q),$e(Ct,"id",Ve),$e(se,"for",Ne),y(ye,ve),$e(Fe,"id",gt),$e(Qe,"for",st),y($t,be),$e(vt,"id",_e),$e(vt,"placeholder",dt),y(Vt,Tt),$e(Pt,"aria-label",xr),bt(Pt,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a($)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),bt(Rt,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a($)?"translate-x-4":"translate-x-1"}`),y(Kt,kr),y(_t,qt),Xe.disabled=a(_),y(x,Qt)},[()=>p("hooks.newHook"),()=>U(I,"event"),()=>p("hooks.event"),()=>U(I,"event"),()=>U(I,"timeout"),()=>p("hooks.timeout"),()=>U(I,"timeout"),()=>U(I,"command"),()=>p("hooks.command"),()=>U(I,"command"),()=>p("hooks.commandPlaceholder"),()=>p("hooks.enabled"),()=>p("hooks.enabled"),()=>p("hooks.globalToggleHint"),()=>p("hooks.cancel"),()=>a(_)?p("hooks.saving"):p("hooks.save")]),Sn(Ct,()=>a(M),D=>c(M,D)),Gr(Fe,()=>a(S),D=>c(S,D)),Gr(vt,()=>a(O),D=>c(O,D)),te("click",Ue,()=>{c(u,!1),c(d,""),B()}),te("click",Xe,xe),b(H,ee)};q(jt,H=>{a(u)&&H(It)})}var St=g(jt,2);{var Ut=H=>{var ee=Yf(),Ae=i(ee);N(Ye=>y(Ae,Ye),[()=>p("hooks.loading")]),b(H,ee)},C=H=>{var ee=Qf(),Ae=i(ee);N(()=>y(Ae,a(l))),b(H,ee)},z=H=>{var ee=Xf(),Ae=i(ee);N(Ye=>y(Ae,Ye),[()=>p("hooks.noHooks")]),b(H,ee)},K=H=>{var ee=sv(),Ae=ge(ee);{var Ye=He=>{var ue=Zf(),ze=i(ue);N(()=>y(ze,a(d))),b(He,ue)};q(Ae,He=>{a(d)&&He(Ye)})}var De=g(Ae,2);lt(De,21,()=>a(n),He=>He.id,(He,ue)=>{var ze=nv(),Ct=i(ze);{var ce=ye=>{var Fe=rv(),oe=i(Fe),Qe=i(oe),$t=i(Qe),vt=i($t),ht=g($t,2);lt(ht,21,()=>r,zt,(_e,dt)=>{var Tt=ev(),xr=i(Tt),kr={};N(qt=>{y(xr,qt),kr!==(kr=a(dt))&&(Tt.value=(Tt.__value=a(dt))??"")},[()=>E(a(dt))]),b(_e,Tt)});var Et=g(Qe,2),Vt=i(Et),Pt=i(Vt),Rt=g(Vt,2),Dt=g(Et,2),Kt=i(Dt),G=i(Kt),ie=g(Kt,2),fe=g(Dt,2),Ue=i(fe),_t=i(Ue),Xe=g(Ue,2),x=i(Xe),D=g(Xe,2),V=i(D),Q=g(oe,2);{var Ve=_e=>{var dt=tv(),Tt=i(dt);N(()=>y(Tt,a(d))),b(_e,dt)};q(Q,_e=>{a(d)&&_e(Ve)})}var Ne=g(Q,2),ve=i(Ne),gt=i(ve),st=g(ve,2),be=i(st);N((_e,dt,Tt,xr,kr,qt,Qt,pr,Zr,Xt,Bt,h,f,P)=>{$e($t,"for",_e),y(vt,dt),$e(ht,"id",Tt),$e(Vt,"for",xr),y(Pt,kr),$e(Rt,"id",qt),$e(Kt,"for",Qt),y(G,pr),$e(ie,"id",Zr),y(_t,Xt),$e(Xe,"aria-label",Bt),bt(Xe,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a($)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),bt(x,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a($)?"translate-x-4":"translate-x-1"}`),y(V,h),y(gt,f),st.disabled=a(_),y(be,P)},[()=>U(a(ue).id,"event"),()=>p("hooks.event"),()=>U(a(ue).id,"event"),()=>U(a(ue).id,"timeout"),()=>p("hooks.timeout"),()=>U(a(ue).id,"timeout"),()=>U(a(ue).id,"command"),()=>p("hooks.command"),()=>U(a(ue).id,"command"),()=>p("hooks.enabled"),()=>p("hooks.enabled"),()=>p("hooks.globalToggleHint"),()=>p("hooks.cancel"),()=>a(_)?p("hooks.saving"):p("hooks.save")]),Sn(ht,()=>a(M),_e=>c(M,_e)),Gr(Rt,()=>a(S),_e=>c(S,_e)),Gr(ie,()=>a(O),_e=>c(O,_e)),te("click",ve,J),te("click",st,()=>ne(a(ue).id)),b(ye,Fe)},se=ye=>{var Fe=av(),oe=i(Fe),Qe=i(oe),$t=i(Qe),vt=i($t),ht=g($t,2),Et=i(ht),Vt=g(Qe,2),Pt=i(Vt),Rt=g(Vt,2),Dt=i(Rt),Kt=g(oe,2),G=i(Kt),ie=i(G),fe=g(G,2),Ue=i(fe);N((_t,Xe,x,D,V)=>{y(vt,_t),bt(ht,1,`rounded-full px-2 py-1 text-xs font-medium ${a(s)?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(Et,Xe),y(Pt,a(ue).command),y(Dt,`${x??""}: ${a(ue).timeout_ms??""}ms`),y(ie,D),fe.disabled=a(w)===a(ue).id,y(Ue,V)},[()=>E(a(ue).event),()=>a(s)?p("common.enabled"):p("common.disabled"),()=>p("hooks.timeout"),()=>p("hooks.edit"),()=>a(w)===a(ue).id?p("hooks.deleting"):p("hooks.delete")]),te("click",G,()=>R(a(ue))),te("click",fe,()=>Ee(a(ue).id)),b(ye,Fe)};q(Ct,ye=>{a(v)===a(ue).id?ye(ce):ye(se,-1)})}b(He,ze)}),b(H,ee)};q(St,H=>{a(o)?H(Ut):a(l)?H(C,1):a(n).length===0?H(z,2):H(K,-1)})}N((H,ee,Ae,Ye,De)=>{y(le,H),y(Y,ee),y(Mt,Ae),X.disabled=a(n).length===0||a(m)!=="",$e(X,"aria-label",Ye),bt(X,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(s)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),bt(we,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(s)?"translate-x-4":"translate-x-1"}`),y(ft,De)},[()=>p("hooks.title"),()=>a(u)?p("hooks.cancelAdd"):p("hooks.addHook"),()=>p("hooks.globalStatus"),()=>a(s)?p("common.disabled"):p("common.enabled"),()=>p("hooks.globalToggleHint")]),te("click",je,()=>{c(u,!a(u)),c(d,""),a(u)&&B()}),te("click",X,()=>{var H;return Te(((H=a(n)[0])==null?void 0:H.id)??"")}),b(e,W),he()}Hr(["click"]);var lv=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),dv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),cv=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),uv=k('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),fv=k('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),vv=k('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),gv=k('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),pv=k('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),hv=k('<div class="space-y-4"></div>'),yv=k('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function bv(e,t){pe(t,!0);let r=L(it([])),n=L(!0),s=L(""),o=L(null);async function l(){c(n,!0);try{const E=await kt.getMcpServers();c(r,Array.isArray(E==null?void 0:E.servers)?E.servers:[],!0),c(s,"")}catch(E){c(r,[],!0),c(s,E instanceof Error?E.message:p("mcp.loadFailed"),!0)}finally{c(n,!1)}}function d(E){c(o,a(o)===E?null:E,!0)}async function v(){await l()}ar(()=>{l()});var u=yv(),_=i(u),w=i(_),m=i(w),I=g(w,2),M=i(I),O=g(_,2);{var S=E=>{var A=lv(),F=i(A);N(R=>y(F,R),[()=>p("mcp.loading")]),b(E,A)},$=E=>{var A=dv(),F=i(A);N(()=>y(F,a(s))),b(E,A)},B=E=>{var A=cv(),F=i(A);N(R=>y(F,R),[()=>p("mcp.noServers")]),b(E,A)},U=E=>{var A=hv();lt(A,21,()=>a(r),zt,(F,R)=>{var J=pv(),ne=i(J),xe=i(ne),Ee=i(xe),Te=i(Ee),W=i(Te),ae=g(Te,2),de=i(ae),le=g(Ee,2),je=i(le),Y=g(xe,2),ke=i(Y),We=g(ne,2);{var Mt=we=>{var Ge=vv(),ft=i(Ge),jt=i(ft),It=g(ft,2);lt(It,21,()=>a(R).tools,zt,(St,Ut)=>{var C=fv(),z=i(C),K=i(z),H=g(z,2);{var ee=Ae=>{var Ye=uv(),De=i(Ye);N(()=>y(De,a(Ut).description)),b(Ae,Ye)};q(H,Ae=>{a(Ut).description&&Ae(ee)})}N(()=>y(K,a(Ut).name)),b(St,C)}),N(St=>y(jt,St),[()=>p("mcp.availableTools")]),b(we,Ge)},X=we=>{var Ge=gv(),ft=i(Ge),jt=i(ft);N(It=>y(jt,It),[()=>p("mcp.noTools")]),b(we,Ge)};q(We,we=>{a(o)===a(R).name&&a(R).tools&&a(R).tools.length>0?we(Mt):a(o)===a(R).name&&(!a(R).tools||a(R).tools.length===0)&&we(X,1)})}N((we,Ge)=>{var ft;y(W,a(R).name),bt(ae,1,`rounded-full px-2 py-1 text-xs font-medium ${a(R).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":a(R).status==="connecting"?"border border-yellow-500/50 bg-yellow-500/20 text-yellow-700 dark:text-yellow-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(de,we),y(je,a(R).url),y(ke,`${((ft=a(R).tools)==null?void 0:ft.length)??0??""} ${Ge??""}`)},[()=>a(R).status==="connected"?p("mcp.connected"):a(R).status==="connecting"?p("mcp.connecting"):p("mcp.disconnected"),()=>p("mcp.tools")]),te("click",ne,()=>d(a(R).name)),b(F,J)}),b(E,A)};q(O,E=>{a(n)?E(S):a(s)?E($,1):a(r).length===0?E(B,2):E(U,-1)})}N((E,A)=>{y(m,E),y(M,A)},[()=>p("mcp.title"),()=>p("common.refresh")]),te("click",I,v),b(e,u),he()}Hr(["click"]);var _v=k('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),mv=k("<div> </div>"),xv=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),kv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),wv=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Sv=k('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),$v=k('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),Ev=k('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),Av=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between gap-3"><div class="flex items-center gap-2"><span> </span> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></div> <!></div></article>'),Mv=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Cv=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Pv=k('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Tv=k('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Nv=k('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Ov=k('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),Fv=k('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),Iv=k('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),Lv=k('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),Rv=k('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),jv=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Dv=k('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option>GitHub</option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),Hv=k('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function zv(e,t){pe(t,!0);let r=L("installed"),n=L(it([])),s=L(!0),o=L(""),l=L(""),d=L("success"),v=L(it([])),u=L(!1),_=L(""),w=L(""),m=L("github"),I=L(!1),M=L(""),O=L(""),S=L("");function $(C,z="success"){c(l,C,!0),c(d,z,!0),setTimeout(()=>{c(l,"")},3e3)}async function B(){try{const C=await kt.getSkills();c(n,Array.isArray(C==null?void 0:C.skills)?C.skills:[],!0),c(o,"")}catch(C){c(n,[],!0),c(o,C instanceof Error?C.message:p("skills.loadFailed"),!0)}finally{c(s,!1)}}async function U(C){if(a(S)!==C){c(S,C,!0);return}c(S,""),c(O,C,!0);try{await kt.uninstallSkill(C),c(n,a(n).filter(z=>z.name!==C),!0),$(p("skills.uninstallSuccess"))}catch(z){$(p("skills.uninstallFailed")+(z!=null&&z.message?`: ${z.message}`:""),"error")}finally{c(O,"")}}const E=Ze(()=>[...a(n)].sort((C,z)=>C.enabled===z.enabled?0:C.enabled?-1:1)),A=Ze(()=>a(n).filter(C=>C.enabled).length);async function F(){!a(w).trim()&&a(m)==="github"&&c(w,"agent skill"),c(u,!0),c(I,!0),c(_,"");try{const C=await kt.discoverSkills(a(m),a(w));c(v,Array.isArray(C==null?void 0:C.results)?C.results:[],!0)}catch(C){c(v,[],!0),c(_,C instanceof Error?C.message:p("skills.searchFailed"),!0)}finally{c(u,!1)}}function R(C){return a(n).some(z=>z.name===C)}async function J(C,z){c(M,C,!0);try{const K=await kt.installSkill(C,z);K!=null&&K.skill&&c(n,[...a(n),{...K.skill,enabled:!0}],!0),$(p("skills.installSuccess"))}catch(K){$(p("skills.installFailed")+(K!=null&&K.message?`: ${K.message}`:""),"error")}finally{c(M,"")}}function ne(C){C.key==="Enter"&&F()}ar(()=>{B()});var xe=Hv(),Ee=i(xe),Te=i(Ee),W=i(Te),ae=i(W),de=g(W,2);{var le=C=>{var z=_v(),K=i(z);N(H=>y(K,`${a(A)??""}/${a(n).length??""} ${H??""}`),[()=>p("skills.active")]),b(C,z)};q(de,C=>{!a(s)&&a(n).length>0&&C(le)})}var je=g(Te,2),Y=i(je),ke=g(Ee,2),We=i(ke),Mt=i(We),X=g(We,2),we=i(X),Ge=g(ke,2);{var ft=C=>{var z=mv(),K=i(z);N(()=>{bt(z,1,`rounded-lg px-4 py-2 text-sm ${a(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),y(K,a(l))}),b(C,z)};q(Ge,C=>{a(l)&&C(ft)})}var jt=g(Ge,2);{var It=C=>{var z=Oe(),K=ge(z);{var H=De=>{var He=xv(),ue=i(He);N(ze=>y(ue,ze),[()=>p("skills.loading")]),b(De,He)},ee=De=>{var He=kv(),ue=i(He);N(()=>y(ue,a(o))),b(De,He)},Ae=De=>{var He=wv(),ue=i(He);N(ze=>y(ue,ze),[()=>p("skills.noSkills")]),b(De,He)},Ye=De=>{var He=Mv();lt(He,21,()=>a(E),zt,(ue,ze)=>{var Ct=Av(),ce=i(Ct),se=i(ce),ye=i(se),Fe=g(ce,2);{var oe=ie=>{var fe=Sv(),Ue=i(fe);N(()=>y(Ue,a(ze).description)),b(ie,fe)};q(Fe,ie=>{a(ze).description&&ie(oe)})}var Qe=g(Fe,2),$t=i(Qe),vt=g(Qe,2),ht=i(vt),Et=i(ht),Vt=i(Et),Pt=g(Et,2),Rt=i(Pt),Dt=g(ht,2);{var Kt=ie=>{var fe=$v(),Ue=i(fe),_t=i(Ue),Xe=g(Ue,2),x=i(Xe),D=g(Xe,2),V=i(D);N((Q,Ve,Ne)=>{y(_t,Q),Xe.disabled=a(O)===a(ze).name,y(x,Ve),y(V,Ne)},[()=>p("skills.confirmUninstall").replace("{name}",a(ze).name),()=>a(O)===a(ze).name?p("skills.uninstalling"):p("common.yes"),()=>p("common.no")]),te("click",Xe,()=>U(a(ze).name)),te("click",D,()=>{c(S,"")}),b(ie,fe)},G=ie=>{var fe=Ev(),Ue=i(fe);N(_t=>y(Ue,_t),[()=>p("skills.uninstall")]),te("click",fe,()=>U(a(ze).name)),b(ie,fe)};q(Dt,ie=>{a(S)===a(ze).name?ie(Kt):ie(G,-1)})}N((ie,fe)=>{y(ye,a(ze).name),y($t,a(ze).location),bt(Et,1,`rounded-full px-2 py-1 text-xs font-medium ${a(ze).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),y(Vt,ie),y(Rt,fe)},[()=>a(ze).enabled?p("common.enabled"):p("common.disabled"),()=>p("skills.readOnlyState")]),b(ue,Ct)}),b(De,He)};q(K,De=>{a(s)?De(H):a(o)?De(ee,1):a(n).length===0?De(Ae,2):De(Ye,-1)})}b(C,z)};q(jt,C=>{a(r)==="installed"&&C(It)})}var St=g(jt,2);{var Ut=C=>{var z=Dv(),K=ge(z),H=i(K),ee=i(H);ee.value=ee.__value="github";var Ae=g(H,2),Ye=g(Ae,2),De=i(Ye),He=g(K,2);{var ue=se=>{var ye=Cv(),Fe=i(ye);N(oe=>y(Fe,oe),[()=>p("skills.searching")]),b(se,ye)},ze=se=>{var ye=Pv(),Fe=i(ye);N(()=>y(Fe,a(_))),b(se,ye)},Ct=se=>{var ye=Tv(),Fe=i(ye);N(oe=>y(Fe,oe),[()=>p("skills.noResults")]),b(se,ye)},ce=se=>{var ye=jv();lt(ye,21,()=>a(v),zt,(Fe,oe)=>{const Qe=Ze(()=>R(a(oe).name));var $t=Rv(),vt=i($t),ht=i(vt),Et=i(ht),Vt=i(Et),Pt=g(Et,2),Rt=i(Pt),Dt=g(ht,2),Kt=i(Dt),G=g(vt,2);{var ie=be=>{var _e=Nv(),dt=i(_e);N(()=>y(dt,a(oe).description)),b(be,_e)};q(G,be=>{a(oe).description&&be(ie)})}var fe=g(G,2),Ue=i(fe);{var _t=be=>{var _e=Ov(),dt=g(i(_e));N(()=>y(dt,` ${a(oe).stars??""}`)),b(be,_e)};q(Ue,be=>{a(oe).stars>0&&be(_t)})}var Xe=g(Ue,2);{var x=be=>{var _e=Fv(),dt=i(_e);N(()=>y(dt,a(oe).language)),b(be,_e)};q(Xe,be=>{a(oe).language&&be(x)})}var D=g(Xe,2),V=i(D),Q=g(fe,2),Ve=i(Q),Ne=i(Ve),ve=g(Ve,2);{var gt=be=>{var _e=Iv(),dt=i(_e);N(Tt=>y(dt,Tt),[()=>p("skills.installed")]),b(be,_e)},st=be=>{var _e=Lv(),dt=i(_e);N(Tt=>{_e.disabled=a(M)===a(oe).url,y(dt,Tt)},[()=>a(M)===a(oe).url?p("skills.installing"):p("skills.install")]),te("click",_e,()=>J(a(oe).url,a(oe).name)),b(be,_e)};q(ve,be=>{a(Qe)?be(gt):be(st,-1)})}N((be,_e,dt)=>{y(Vt,a(oe).name),y(Rt,`${be??""} ${a(oe).owner??""}`),y(Kt,a(oe).source),bt(D,1,Ks(a(oe).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400")),y(V,_e),$e(Ve,"href",a(oe).url),y(Ne,dt)},[()=>p("skills.owner"),()=>a(oe).has_license?p("skills.licensed"):p("skills.unlicensed"),()=>a(oe).url.replace("https://github.com/","")]),b(Fe,$t)}),b(se,ye)};q(He,se=>{a(u)?se(ue):a(_)?se(ze,1):a(I)&&a(v).length===0?se(Ct,2):a(v).length>0&&se(ce,3)})}N((se,ye)=>{$e(Ae,"placeholder",se),Ye.disabled=a(u),y(De,ye)},[()=>p("skills.search"),()=>a(u)?p("skills.searching"):p("skills.searchBtn")]),Sn(H,()=>a(m),se=>c(m,se)),te("keydown",Ae,ne),Gr(Ae,()=>a(w),se=>c(w,se)),te("click",Ye,F),b(C,z)};q(St,C=>{a(r)==="discover"&&C(Ut)})}N((C,z,K,H)=>{y(ae,C),y(Y,z),bt(We,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),y(Mt,K),bt(X,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),y(we,H)},[()=>p("skills.title"),()=>p("common.refresh"),()=>p("skills.tabInstalled"),()=>p("skills.tabDiscover")]),te("click",je,()=>{c(s,!0),B()}),te("click",We,()=>{c(r,"installed")}),te("click",X,()=>{c(r,"discover")}),b(e,xe),he()}Hr(["click","keydown"]);var Uv=k("<div> </div>"),Vv=k('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Kv=k('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),qv=k('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),Bv=k('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),Wv=k('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),Gv=k('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),Jv=k('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),Yv=k('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),Qv=k('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),Xv=k('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Zv=k('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function e0(e,t){pe(t,!0);let r=L(it([])),n=L(!0),s=L(""),o=L(""),l=L(""),d=L("success");function v(W,ae="success"){c(l,W,!0),c(d,ae,!0),setTimeout(()=>{c(l,"")},3e3)}async function u(){c(n,!0);try{const W=await kt.getPlugins();c(r,Array.isArray(W==null?void 0:W.plugins)?W.plugins:[],!0),c(s,"")}catch{c(r,[],!0),c(s,p("plugins.loadFailed"),!0)}finally{c(n,!1)}}async function _(W){c(o,W,!0);try{await kt.reloadPlugin(W),v(p("plugins.reloadSuccess",{name:W})),await u()}catch(ae){v(p("plugins.reloadFailed")+(ae.message?`: ${ae.message}`:""),"error")}finally{c(o,"")}}function w(W){return typeof W=="string"&&W==="Active"?"text-green-500":typeof W=="object"&&(W!=null&&W.Error)?"text-red-500":"text-yellow-500"}function m(W){return typeof W=="string"&&W==="Active"?p("plugins.statusActive"):typeof W=="object"&&(W!=null&&W.Error)?W.Error:p("common.unknown")}ar(()=>{u()});var I=Zv(),M=ge(I);{var O=W=>{var ae=Uv(),de=i(ae);N(()=>{bt(ae,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${a(d)==="error"?"bg-red-600":"bg-green-600"}`),y(de,a(l))}),b(W,ae)};q(M,W=>{a(l)&&W(O)})}var S=g(M,2),$=i(S),B=i($),U=i(B);mo(U,{size:24});var E=g(U,2),A=i(E),F=g(B,2),R=i(F),J=g($,2);{var ne=W=>{var ae=Vv(),de=i(ae);N(le=>y(de,le),[()=>p("plugins.loading")]),b(W,ae)},xe=W=>{var ae=Kv(),de=i(ae);N(()=>y(de,a(s))),b(W,ae)},Ee=W=>{var ae=qv(),de=i(ae);mo(de,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var le=g(de,2),je=i(le);N(Y=>y(je,Y),[()=>p("plugins.noPlugins")]),b(W,ae)},Te=W=>{var ae=Xv();lt(ae,21,()=>a(r),zt,(de,le)=>{var je=Qv(),Y=i(je),ke=i(Y),We=i(ke),Mt=i(We),X=g(We,2),we=i(X),Ge=g(ke,2),ft=i(Ge);{var jt=ce=>{Fc(ce,{size:16})},It=ce=>{Oc(ce,{size:16})};q(ft,ce=>{typeof a(le).status=="string"&&a(le).status==="Active"?ce(jt):ce(It,-1)})}var St=g(ft,2),Ut=i(St),C=g(Y,2);{var z=ce=>{var se=Bv(),ye=i(se);N(()=>y(ye,a(le).description)),b(ce,se)};q(C,ce=>{a(le).description&&ce(z)})}var K=g(C,2);{var H=ce=>{var se=Gv(),ye=i(se),Fe=i(ye),oe=g(ye,2);lt(oe,21,()=>a(le).capabilities,zt,(Qe,$t)=>{var vt=Wv(),ht=i(vt);N(()=>y(ht,a($t))),b(Qe,vt)}),N(Qe=>y(Fe,Qe),[()=>p("plugins.capabilities")]),b(ce,se)};q(K,ce=>{var se;(se=a(le).capabilities)!=null&&se.length&&ce(H)})}var ee=g(K,2);{var Ae=ce=>{var se=Yv(),ye=i(se),Fe=i(ye),oe=g(ye,2);lt(oe,21,()=>a(le).permissions_required,zt,(Qe,$t)=>{var vt=Jv(),ht=i(vt);N(()=>y(ht,a($t))),b(Qe,vt)}),N(Qe=>y(Fe,Qe),[()=>p("plugins.permissions")]),b(ce,se)};q(ee,ce=>{var se;(se=a(le).permissions_required)!=null&&se.length&&ce(Ae)})}var Ye=g(ee,2),De=i(Ye),He=i(De);{var ue=ce=>{Bc(ce,{size:14,class:"animate-spin"})},ze=ce=>{Ns(ce,{size:14})};q(He,ce=>{a(o)===a(le).name?ce(ue):ce(ze,-1)})}var Ct=g(He);N((ce,se,ye)=>{y(Mt,a(le).name),y(we,`v${a(le).version??""}`),bt(Ge,1,`flex items-center gap-1 ${ce??""}`),y(Ut,se),De.disabled=a(o)===a(le).name,y(Ct,` ${ye??""}`)},[()=>w(a(le).status),()=>m(a(le).status),()=>p("plugins.reload")]),te("click",De,()=>_(a(le).name)),b(de,je)}),b(W,ae)};q(J,W=>{a(n)?W(ne):a(s)?W(xe,1):a(r).length===0?W(Ee,2):W(Te,-1)})}N((W,ae)=>{y(A,W),y(R,ae)},[()=>p("plugins.title"),()=>p("common.refresh")]),te("click",F,u),b(e,I),he()}Hr(["click"]);var t0=k('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),r0=k('<button type="button"> </button>'),a0=k('<p class="px-2 py-1 text-xs text-gray-400 dark:text-gray-500"> </p>'),n0=k('<div class="ml-4 mt-1 space-y-1 border-l border-gray-200 pl-3 dark:border-gray-700"><!> <!></div>'),s0=k('<button type="button"> </button> <!>',1),o0=k('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),i0=k('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="console-header sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" aria-label="Toggle theme" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),l0=k('<div class="console-shell min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function d0(e,t){pe(t,!0);let r=L(it($i())),n=L(it(Vn())),s=L(!1);const o="prx-console-theme";let l=L("system"),d=L(!0),v=L(it([])),u=L(!1),_=L(it(typeof window<"u"?window.location.hash:""));const w=Ze(()=>a(n).length>0),m=Ze(()=>a(w)&&a(r)==="/"?"/overview":a(r)),I=Ze(()=>a(m).startsWith("/chat/")?"/sessions":a(m)),M=Ze(()=>a(m)==="/config"),O=Ze(()=>a(_).startsWith("#config-section-")?a(_).slice(16):"");function S(Y){try{return decodeURIComponent(Y)}catch{return Y}}const $=Ze(()=>a(m).startsWith("/chat/")?S(a(m).slice(6)):"");function B(){return a(l)==="dark"?!0:a(l)==="light"?!1:window.matchMedia("(prefers-color-scheme: dark)").matches}function U(){const Y=localStorage.getItem(o);c(l,Y==="light"||Y==="dark"?Y:"system",!0),E()}function E(){const Y=B();c(d,Y,!0),document.documentElement.classList.toggle("dark",Y),document.documentElement.classList.toggle("light",!Y),document.documentElement.style.colorScheme=Y?"dark":"light"}function A(){c(l,a(d)?"light":"dark",!0),localStorage.setItem(o,a(l)),E()}function F(){c(n,Vn(),!0),ho()}function R(Y){c(r,Y,!0),c(s,!1),c(_,typeof window<"u"?window.location.hash:"",!0)}function J(Y){c(n,Y,!0),la("/overview",!0)}function ne(){Si(),c(n,""),la("/",!0)}function xe(Y){la(Y)}function Ee(){c(_,window.location.hash,!0)}async function Te(){if(!(!a(w)||a(m)!=="/config"||a(u))){c(u,!0);try{const Y=await Ai();c(v,mn(Y),!0)}catch{c(v,mn(null),!0)}finally{c(u,!1)}}}function W(Y){Ei(Y),c(s,!1)}ar(()=>{U(),ho();const Y=Hd(R),ke=window.matchMedia("(prefers-color-scheme: dark)"),We=X=>{if(X.key==="prx-console-token"){F();return}if(X.key===ts&&Sc(),X.key===o){const we=localStorage.getItem(o);c(l,we==="light"||we==="dark"?we:"system",!0),E()}},Mt=()=>{a(l)==="system"&&E()};return window.addEventListener("storage",We),window.addEventListener("hashchange",Ee),ke.addEventListener("change",Mt),()=>{Y(),window.removeEventListener("storage",We),window.removeEventListener("hashchange",Ee),ke.removeEventListener("change",Mt)}}),ar(()=>{if(a(w)&&a(r)==="/"){la("/overview",!0);return}!a(w)&&a(r)!=="/"&&la("/",!0)}),ar(()=>{if(a(M)){Zt.data&&c(v,mn(Zt.data),!0),Te();return}c(v,[],!0)});var ae=l0(),de=i(ae);{var le=Y=>{ru(Y,{onLogin:J})},je=Y=>{var ke=i0(),We=i(ke);{var Mt=G=>{var ie=t0();N(fe=>$e(ie,"aria-label",fe),[()=>p("app.closeSidebar")]),te("click",ie,()=>c(s,!1)),b(G,ie)};q(We,G=>{a(s)&&G(Mt)})}var X=g(We,2),we=i(X),Ge=i(we),ft=i(Ge),jt=g(we,2);lt(jt,21,()=>Rd,zt,(G,ie)=>{var fe=s0(),Ue=ge(fe),_t=i(Ue),Xe=g(Ue,2);{var x=D=>{var V=n0(),Q=i(V);lt(Q,17,()=>a(v),zt,(ve,gt)=>{var st=r0(),be=i(st);N(()=>{bt(st,1,`w-full rounded-md px-2 py-1.5 text-left text-xs transition ${a(O)===a(gt).groupKey?"bg-sky-50 text-sky-700 dark:bg-sky-500/10 dark:text-sky-300":"text-gray-500 hover:bg-gray-100 hover:text-gray-800 dark:text-gray-400 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),y(be,a(gt).label)}),te("click",st,()=>W(a(gt).groupKey)),b(ve,st)});var Ve=g(Q,2);{var Ne=ve=>{var gt=a0(),st=i(gt);N(be=>y(st,be),[()=>p("common.loading")]),b(ve,gt)};q(Ve,ve=>{a(u)&&a(v).length===0&&ve(Ne)})}b(D,V)};q(Xe,D=>{a(ie).path==="/config"&&a(M)&&D(x)})}N(D=>{bt(Ue,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${a(I)===a(ie).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),y(_t,D)},[()=>p(a(ie).labelKey)]),te("click",Ue,()=>xe(a(ie).path)),b(G,fe)});var It=g(X,2),St=i(It),Ut=i(St),C=i(Ut),z=i(C),K=g(C,2),H=i(K),ee=g(Ut,2),Ae=i(ee),Ye=i(Ae);{var De=G=>{Xc(G,{size:16})},He=G=>{Gc(G,{size:16})};q(Ye,G=>{a(d)?G(De):G(He,-1)})}var ue=g(Ae,2),ze=i(ue),Ct=g(ue,2),ce=i(Ct),se=g(St,2),ye=i(se);{var Fe=G=>{vu(G,{})},oe=G=>{Su(G,{})},Qe=G=>{zu(G,{get sessionId(){return a($)}})},$t=Ze(()=>a(m).startsWith("/chat/")),vt=G=>{Ju(G,{})},ht=G=>{iv(G,{})},Et=G=>{bv(G,{})},Vt=G=>{zv(G,{})},Pt=G=>{e0(G,{})},Rt=G=>{zf(G,{})},Dt=G=>{Bf(G,{})},Kt=G=>{var ie=o0(),fe=i(ie),Ue=i(fe),_t=g(fe,2),Xe=i(_t);N((x,D)=>{y(Ue,x),y(Xe,D)},[()=>p("app.notFound"),()=>p("app.backToOverview")]),te("click",_t,()=>xe("/overview")),b(G,ie)};q(ye,G=>{a(m)==="/overview"?G(Fe):a(m)==="/sessions"?G(oe,1):a($t)?G(Qe,2):a(m)==="/channels"?G(vt,3):a(m)==="/hooks"?G(ht,4):a(m)==="/mcp"?G(Et,5):a(m)==="/skills"?G(Vt,6):a(m)==="/plugins"?G(Pt,7):a(m)==="/config"?G(Rt,8):a(m)==="/logs"?G(Dt,9):G(Kt,-1)})}N((G,ie,fe,Ue,_t,Xe)=>{bt(X,1,`console-sidebar fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${a(s)?"translate-x-0":"-translate-x-full"}`),y(ft,G),y(z,ie),y(H,fe),$e(ue,"aria-label",Ue),y(ze,_t),y(ce,Xe)},[()=>p("app.title"),()=>p("app.menu"),()=>p("app.title"),()=>p("app.language"),()=>p("app.languageToggle"),()=>p("common.logout")]),te("click",C,()=>c(s,!a(s))),te("click",Ae,A),te("click",ue,function(...G){Da==null||Da.apply(this,G)}),te("click",Ct,ne),b(Y,ke)};q(de,Y=>{a(w)?Y(je,-1):Y(le)})}b(e,ae),he()}Hr(["click"]);ld(d0,{target:document.getElementById("app")});
