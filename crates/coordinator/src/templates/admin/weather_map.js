// Progressive enhancement only: forecasts, basemap and ordinary forms are server rendered.
function initWeatherMaps() {
  document.querySelectorAll('.weather-map:not([data-ready])').forEach(map => {
    map.dataset.ready = 'true';
    const form = map.closest('form'), svg = map.querySelector('svg');
    const layer = map.querySelector('[data-map-layer]'), time = map.querySelector('[data-map-time]');
    const wind = map.querySelector('[data-map-wind]'), inspector = map.querySelector('[data-map-inspector]');
    const markers = [...map.querySelectorAll('[data-station]')];
    const initial = svg.getAttribute('viewBox').split(' ').map(Number);
    // Outside the game form (the map's own page) station links keep their nearby search.
    const inputs = new Map(form ? [...form.querySelectorAll('input[name="locations"]')].map(el => [el.value, el]) : []);
    const records = new Map(markers.map(el => [el, JSON.parse(el.dataset.forecasts).map(f => ({...f, start: Date.parse(f.start_time), end: Date.parse(f.end_time)}))]));
    const palette = ['#6865c7','#408cca','#39a99b','#dfbb4d','#ed853c','#d34857'];
    const scales = {high:[32,50,65,80,95], low:[32,50,65,80,95], wind:[5,10,20,30,40], rain:[10,25,50,75,90]};
    const units = {high:'°F',low:'°F',wind:'mph',rain:'%'};
    map.querySelectorAll('.map-controls [disabled]').forEach(el => el.disabled = false);
    function reading(el) {
      if (!time.value) return {high:Number(el.dataset.high),low:Number(el.dataset.low),wind:el.dataset.wind ? Number(el.dataset.wind)*1.15078 : null,rain:el.dataset.rain ? Number(el.dataset.rain) : null,period:'whole game window',direction:null};
      const at = Number(time.value);
      // Narrowest containing interval; a daily range never becomes an hourly temperature.
      const row = records.get(el).filter(f => f.start <= at && at < f.end).sort((a,b) => (a.end-a.start)-(b.end-b.start) || b.start-a.start || JSON.stringify(a).localeCompare(JSON.stringify(b)))[0];
      if (!row) return {period:'no forecast covering this time'};
      return {high:row.temp_high,low:row.temp_low,wind:row.wind_speed == null ? null : row.wind_speed*1.15078,rain:row.precip_chance,direction:row.wind_direction,period:`${row.start_time} → ${row.end_time}`};
    }
    const description = el => {
      const f = reading(el), val = f[layer.value];
      return `${el.dataset.station} · ${el.dataset.name} · ${val == null ? 'unknown' : Math.round(val)+' '+units[layer.value]} · ${f.period}`;
    };
    function render() {
      const thresholds = scales[layer.value];
      const legend = map.querySelector('[data-map-legend]');
      legend.replaceChildren();
      [...thresholds.map((n,i) => i ? `${thresholds[i-1]}–<${n}` : `<${n}`),`≥${thresholds[4]}`,'Unknown'].forEach((label,i) => {
        const item = document.createElement('span'), swatch = document.createElement('i');
        swatch.style.background = palette[i] || '#9aa7ac'; item.append(swatch, `${label} ${units[layer.value]}`); legend.append(item);
      });
      markers.forEach(el => {
        const f = reading(el), val = f[layer.value], index = thresholds.findIndex(n => val < n);
        el.querySelector('circle').setAttribute('fill',val == null ? '#9aa7ac' : palette[index < 0 ? 5 : index]);
        el.querySelector('title').textContent = description(el);
        const arrow = el.querySelector('.map-wind');
        arrow.style.display = wind.checked && f.wind > 0 && f.direction != null && f.direction >= 0 && f.direction <= 360 ? '' : 'none';
        arrow.removeAttribute('hidden');
        arrow.setAttribute('transform',`rotate(${(f.direction || 0)+180})`);
      });
      inspector.textContent = time.value ? 'Forecasts covering the selected time. Focus a station to see its actual period.' : 'Whole-window extremes. Choose a forecast time to compare wind direction.';
    }
    function selected() {
      if (!form) return;
      markers.forEach(el => el.classList.toggle('selected',!!inputs.get(el.dataset.station)?.checked));
      map.querySelector('[data-map-count]').textContent = `${[...inputs.values()].filter(el => el.checked).length} stations selected · maximum 50`;
    }
    markers.forEach(el => {
      el.addEventListener('pointerenter',() => inspector.textContent = description(el));
      el.addEventListener('focus',() => inspector.textContent = description(el));
      if (form) el.addEventListener('click',event => {
        if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
        event.preventDefault();
        if (map.dataset.usable !== 'true') { inspector.textContent = 'Refresh stale forecasts before selecting stations.'; return; }
        let input = inputs.get(el.dataset.station);
        if (!input) {
          const label = document.createElement('label'); input = document.createElement('input');
          input.type='checkbox';input.name='locations';input.value=el.dataset.station;
          label.append(input,` ${el.dataset.station} · ${el.dataset.name}`);form.querySelector('#map-selections').append(label);inputs.set(input.value,input);
        }
        if (!input.checked && [...inputs.values()].filter(el => el.checked).length >= 50) { inspector.textContent='Select at most 50 stations.'; return; }
        input.checked=!input.checked;input.dispatchEvent(new Event('change',{bubbles:true}));inspector.textContent=description(el)+(input.checked ? ' · added to game' : ' · removed');
      });
    });
    [layer,time,wind].forEach(el => el.addEventListener('change',render));
    form?.addEventListener('change',selected);
    const setView = view => svg.setAttribute('viewBox',view.join(' '));
    map.querySelectorAll('[data-map-zoom]').forEach(button => button.addEventListener('click',() => {
      const [x,y,w,h]=svg.getAttribute('viewBox').split(' ').map(Number), scale=Number(button.dataset.mapZoom);
      if(w*scale<1 || w*scale>360) return;
      setView([x+w*(1-scale)/2,y+h*(1-scale)/2,w*scale,h*scale]);
    }));
    map.querySelector('[data-map-reset]').addEventListener('click',() => setView(initial));
    let drag;
    svg.addEventListener('pointerdown',event => {
      if(event.target.closest('[data-station]')) return;
      drag={x:event.clientX,y:event.clientY,view:svg.getAttribute('viewBox').split(' ').map(Number)};svg.setPointerCapture(event.pointerId);
    });
    svg.addEventListener('pointermove',event => {
      if(!drag) return;
      const [x,y,w,h]=drag.view,scale=Math.max(w/svg.clientWidth,h/svg.clientHeight);
      setView([x-(event.clientX-drag.x)*scale,y-(event.clientY-drag.y)*scale,w,h]);
    });
    svg.addEventListener('pointerup',() => drag=null);svg.addEventListener('pointercancel',() => drag=null);
    render();selected();
  });
}
initWeatherMaps();
// The map arrives after the page, swapped in by htmx.
document.addEventListener('htmx:after:swap',initWeatherMaps);
