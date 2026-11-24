/* 下拉列表渐进增强：原生 select 保存表单值；展示、搜索与键盘导航共用组件。 */
'use strict';
window.SelectControls = (() => {
  const controls = new Map();
  const preferences = new Map();
  let opened = null;
  let sequence = 0;

  /** 创建带固定样式的 DOM 元素，业务文本通过 textContent 设置。 */
  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  /** 读取标签正文，排除原生选项文字和增强控件。 */
  function labelOf(select) {
    if (select.getAttribute('aria-label')) return select.getAttribute('aria-label');
    const label = select.labels?.[0];
    if (!label) return '选择选项';
    const clone = label.cloneNode(true);
    clone.querySelectorAll('select,.select-control').forEach((node) => node.remove());
    return clone.textContent.trim() || '选择选项';
  }

  /** 将当前搜索结果中的活动项滚动到可见范围。 */
  function activate(control, index, scroll = true) {
    if (!control.visible.length) {
      control.active = -1;
      control.trigger.removeAttribute('aria-activedescendant');
      control.search.removeAttribute('aria-activedescendant');
      return;
    }
    control.active = Math.max(0, Math.min(index, control.visible.length - 1));
    control.visible.forEach(({ node }, position) => node.classList.toggle('highlighted', position === control.active));
    const node = control.visible[control.active].node;
    control.trigger.setAttribute('aria-activedescendant', node.id);
    if (control.searchable) control.search.setAttribute('aria-activedescendant', node.id);
    if (scroll) (control.visible[control.active].option ? node : node.firstElementChild).scrollIntoView({ block: 'nearest' });
  }

  /** 从原生选项构建类别与服务树，服务 ID 与名称分离以兼容重名服务。 */
  function optionTree(select) {
    const root = { children: [], options: [] };
    const categories = new Map();
    for (const [index, child] of [...select.children].entries()) {
      if (child.tagName === 'OPTION') {
        if (!child.hidden) { root.children.push({ option: child }); root.options.push(child); }
        continue;
      }
      if (child.tagName !== 'OPTGROUP' || child.hidden) continue;
      const options = [...child.children].filter((option) => option.tagName === 'OPTION' && !option.hidden);
      if (!options.length) continue;
      let parent = root;
      if (child.dataset.category) {
        const category = child.dataset.category;
        if (!categories.has(category)) {
          const group = { key: `category:${category}`, label: category, children: [], options: [] };
          categories.set(category, group);
          root.children.push(group);
        }
        parent = categories.get(category);
        parent.options.push(...options);
      }
      parent.children.push({
        key: `group:${child.dataset.groupId || index}`, label: child.label,
        collapsed: child.dataset.collapsed === 'true', options,
        children: options.map((option) => ({ option })),
      });
      root.options.push(...options);
    }
    return root;
  }

  /** 搜索同时覆盖类别、服务、模型调用名与描述。 */
  function matches(option, term) {
    const group = option.parentElement;
    return `${option.label} ${option.dataset.title || ''} ${option.value} ${group.label || ''} ${group.dataset.category || ''} ${option.dataset.description || ''}`.toLocaleLowerCase().includes(term);
  }

  /** 折叠不修改选中值；搜索临时展开结果，保留原折叠偏好。 */
  function toggleGroup(control, item, expanded = !item.expanded) {
    if (control.search.value.trim()) return;
    control.collapsed.set(item.key, !expanded);
    renderOptions(control, '', item.key);
    position(control);
  }

  /** 给鼠标与键盘注册相同的可见行，避免焦点进入已折叠的模型。 */
  function registerRow(control, item, target = item.node) {
    const index = control.visible.length;
    control.visible.push(item);
    target.addEventListener('pointerdown', (event) => event.preventDefault());
    target.addEventListener('pointermove', () => activate(control, index, false));
    target.addEventListener('click', () => item.option ? choose(control, item.option) : toggleGroup(control, item));
  }

  /** 递归展示树形或普通下拉，选中状态和倍率仍来自原生 option。 */
  function renderBranch(control, branch, host, term, level = 1, parentKey = '') {
    for (const child of branch.children) {
      if (!child.option) {
        const matched = child.options.filter((option) => matches(option, term));
        if (!matched.length) continue;
        if (!control.collapsible) {
          host.append(element('div', 'select-group', child.label));
          renderBranch(control, child, host, term, level, parentKey);
          continue;
        }
        const row = element('div', 'select-branch');
        const heading = element('div', 'select-group-toggle');
        const expanded = term ? true : control.collapsed.has(child.key) ? !control.collapsed.get(child.key)
          : !child.collapsed || child.options.some((option) => option.selected);
        row.id = `${control.list.id}-group-${control.groupSequence++}`;
        row.setAttribute('role', 'treeitem');
        row.setAttribute('aria-level', String(level));
        row.setAttribute('aria-expanded', String(expanded));
        heading.id = `${row.id}-label`;
        row.setAttribute('aria-labelledby', heading.id);
        heading.style.paddingLeft = `${10 + (level - 1) * 14}px`;
        const arrow = element('span', 'select-group-chevron');
        arrow.setAttribute('aria-hidden', 'true');
        heading.append(arrow, element('span', 'select-group-title', child.label),
          element('span', 'select-group-count', term ? `${matched.length}/${child.options.length}` : child.options.length));
        heading.title = term ? '搜索时自动展开匹配分组；清除搜索后恢复折叠' : expanded ? '收起分组' : '展开分组';
        row.append(heading);
        host.append(row);
        registerRow(control, { key: child.key, node: row, expanded, parentKey,
          hasSelection: child.options.some((option) => option.selected) }, heading);
        if (expanded) {
          const children = element('div', 'select-children');
          children.setAttribute('role', 'group');
          row.append(children);
          renderBranch(control, child, children, term, level + 1, child.key);
        }
        continue;
      }
      const option = child.option;
      if (!matches(option, term)) continue;
      const disabled = option.disabled || option.parentElement.disabled === true;
      const row = element('div', 'select-option');
      row.id = `${control.list.id}-option-${option.index}`;
      row.setAttribute('role', control.collapsible ? 'treeitem' : 'option');
      if (control.collapsible) {
        row.setAttribute('aria-level', String(level));
        row.style.paddingLeft = `${10 + (level - 1) * 14}px`;
      }
      row.setAttribute('aria-selected', String(option.selected));
      row.setAttribute('aria-disabled', String(disabled));
      row.title = option.label;
      const texts = element('span', 'select-option-text');
      texts.append(element('span', 'select-option-title', option.dataset.title || option.label));
      if (option.dataset.description) texts.append(element('small', 'select-option-description', option.dataset.description));
      row.append(texts);
      if (option.dataset.badge) {
        const badge = element('span', 'select-option-badge', option.dataset.badge);
        if (option.dataset.originalBadge) badge.prepend(element('s', 'select-option-original', option.dataset.originalBadge));
        badge.title = option.dataset.badgeTitle || '';
        row.append(badge);
      }
      row.append(element('span', 'select-check', option.selected ? '✓' : ''));
      row.addEventListener('pointerdown', (event) => event.preventDefault());
      if (!disabled) registerRow(control, { key: `option:${option.value}`, option, node: row, parentKey });
      host.append(row);
    }
  }

  /** 刷新时保持焦点与滚动位置，只有新搜索才回到结果起点。 */
  function renderOptions(control, query = '', activeKey = null, reset = false) {
    const term = query.trim().toLocaleLowerCase();
    const previous = reset ? null : activeKey || control.visible[control.active]?.key;
    const scrollTop = reset ? 0 : control.list.scrollTop;
    control.list.replaceChildren();
    control.visible = [];
    control.groupSequence = 0;
    const tree = optionTree(control.select);
    renderBranch(control, tree, control.list, term);
    if (!control.list.childElementCount) {
      const message = element('div', 'select-empty', '没有匹配的选项');
      message.setAttribute('role', 'status');
      if (term) {
        const clear = element('button', 'select-clear', '清除搜索');
        clear.type = 'button';
        clear.addEventListener('click', () => {
          control.search.value = '';
          renderOptions(control, '', null, true);
          position(control);
          control.search.focus();
        });
        message.append(clear);
      }
      control.list.append(message);
    }
    const available = tree.options.filter((option) => !option.disabled && option.parentElement.disabled !== true);
    control.count.textContent = term ? `${available.filter((option) => matches(option, term)).length} / ${available.length} 个匹配 · 清除搜索恢复折叠`
      : `${available.length} 个可选项${control.collapsible ? ' · 点击分组或按 ← → 折叠' : ''}`;
    const prior = control.visible.findIndex((item) => item.key === previous);
    const selected = control.visible.findIndex(({ option }) => option?.selected);
    const firstMatch = term ? control.visible.findIndex(({ option }) => option) : -1;
    const selectedGroup = control.visible.findLastIndex((item) => item.hasSelection);
    activate(control, prior >= 0 ? prior : selected >= 0 ? selected : firstMatch >= 0 ? firstMatch : selectedGroup >= 0 ? selectedGroup : 0, false);
    control.list.scrollTop = scrollTop;
  }

  /** 同步原生字段变化，包含选中值、禁用和必填状态。 */
  function sync(control) {
    const selected = control.select.selectedOptions[0];
    const label = selected?.label || '请选择';
    if (control.value.textContent !== label) control.value.textContent = label;
    control.trigger.title = label;
    control.trigger.disabled = control.select.disabled;
    control.trigger.setAttribute('aria-required', String(control.select.required));
    control.trigger.setAttribute('aria-label', `${control.label}：${label}`);
    control.searchable = control.select.options.length > 8 || ['play-model', 'example-model', 'f-preset'].includes(control.select.id);
    control.root.classList.toggle('select-searchable', control.searchable);
    if (control.select.disabled && opened === control) close(false);
    else if (opened === control) {
      renderOptions(control, control.search.value);
      position(control);
    }
  }

  /** 面板在视口内定位；靠近底边时向上展开，不被卡片或对话框裁切。 */
  function position(control) {
    const rect = control.trigger.getBoundingClientRect();
    if (!control.trigger.isConnected || rect.width === 0) { close(false); return; }
    const margin = 10;
    const width = Math.min(Math.max(rect.width, control.searchable ? 310 : 200), window.innerWidth - margin * 2);
    const below = window.innerHeight - rect.bottom - margin - 6;
    const above = rect.top - margin - 6;
    const useAbove = below < 240 && above > below;
    const height = Math.min(380, Math.max(100, useAbove ? above : below));
    control.panel.style.width = `${width}px`;
    control.panel.style.maxHeight = `${height}px`;
    control.panel.style.left = `${Math.max(margin, Math.min(rect.left, window.innerWidth - width - margin))}px`;
    const actual = control.panel.getBoundingClientRect().height;
    control.panel.style.top = `${useAbove ? Math.max(margin, rect.top - actual - 6) : Math.min(rect.bottom + 6, window.innerHeight - actual - margin)}px`;
  }

  /** 关闭当前下拉并可选地还原键盘焦点。 */
  function close(restoreFocus = false) {
    const control = opened;
    if (!control) return;
    opened = null;
    control.panel.hidden = true;
    control.panel.remove();
    control.trigger.setAttribute('aria-expanded', 'false');
    control.search.setAttribute('aria-expanded', 'false');
    control.trigger.removeAttribute('aria-activedescendant');
    control.search.removeAttribute('aria-activedescendant');
    if (restoreFocus && control.trigger.isConnected) control.trigger.focus({ preventScroll: true });
  }

  /** 打开下拉时一次展示相关项，长列表将焦点放在搜索框。 */
  function open(control) {
    if (control.select.disabled) return;
    if (opened === control) { close(true); return; }
    close(false);
    sync(control);
    opened = control;
    control.search.value = '';
    control.search.hidden = !control.searchable;
    control.count.hidden = !control.searchable;
    control.trigger.setAttribute('aria-expanded', 'true');
    control.search.setAttribute('aria-expanded', 'true');
    const host = control.select.closest('dialog[open]') || document.body;
    host.append(control.panel);
    control.panel.hidden = false;
    renderOptions(control, '', null, true);
    position(control);
    if (control.searchable) control.search.focus({ preventScroll: true });
    else control.trigger.focus({ preventScroll: true });
    activate(control, control.active);
  }

  /** 选中选项后触发原生 change，让筛选、表单与请求继续沿用现有逻辑。 */
  function choose(control, option) {
    const changed = control.select.value !== option.value;
    control.select.value = option.value;
    control.trigger.removeAttribute('aria-invalid');
    close(true);
    sync(control);
    if (changed) {
      control.select.dispatchEvent(new Event('input', { bubbles: true }));
      control.select.dispatchEvent(new Event('change', { bubbles: true }));
    }
  }

  /** 树形列表增加左右折叠导航；Escape 只关闭下拉，不冒泡关闭表单。 */
  function keydown(control, event) {
    if (event.key === 'Escape' && opened === control) { event.preventDefault(); event.stopPropagation(); close(true); return; }
    if (event.key === 'Tab' && opened === control) {
      // 搜索框位于浮层中，先还原到触发器再按正常文档顺序移动焦点。
      close(true);
      return;
    }
    if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key) &&
        !(event.target === control.search && ['Home', 'End'].includes(event.key))) {
      event.preventDefault();
      if (opened !== control) { open(control); return; }
      const index = event.key === 'Home' ? 0 : event.key === 'End' ? control.visible.length - 1
        : control.active + (event.key === 'ArrowDown' ? 1 : -1);
      activate(control, index);
      return;
    }
    if (event.key === 'Enter' && opened === control) {
      event.preventDefault();
      const selected = control.visible[control.active];
      if (selected?.option) choose(control, selected.option);
      else if (selected) toggleGroup(control, selected);
      return;
    }
    if (control.collapsible && opened === control && ['ArrowLeft', 'ArrowRight'].includes(event.key)
        && !(event.target === control.search && control.search.value)) {
      event.preventDefault();
      const item = control.visible[control.active];
      if (!item) return;
      if (event.key === 'ArrowRight') {
        if (!item.option && !item.expanded) toggleGroup(control, item, true);
        else if (!item.option) activate(control, control.active + 1);
      } else if (!item.option && item.expanded) toggleGroup(control, item, false);
      else if (item.parentKey) activate(control, control.visible.findIndex((row) => row.key === item.parentKey));
      return;
    }
    if (event.target === control.trigger && event.key.length === 1 && !event.ctrlKey && !event.metaKey && event.key !== ' ') {
      event.preventDefault();
      if (opened !== control) open(control);
      if (control.searchable) {
        control.search.value += event.key;
        renderOptions(control, control.search.value, null, true);
      } else {
        const match = control.visible.findIndex(({ option }) => option?.label.toLocaleLowerCase().startsWith(event.key.toLocaleLowerCase()));
        if (match >= 0) activate(control, match);
      }
    }
  }

  /** 增强一个原生 select，保留 ID、值和原生表单校验。 */
  function enhance(select) {
    const label = labelOf(select);
    const collapsible = select.dataset.collapsible === 'true';
    const role = collapsible ? 'tree' : 'listbox';
    const preferenceKey = select.id || select;
    if (!preferences.has(preferenceKey)) preferences.set(preferenceKey, new Map());
    const root = element('div', 'select-control');
    const trigger = element('button', 'select-trigger');
    const value = element('span', 'select-value');
    const arrow = element('span', 'select-chevron');
    arrow.setAttribute('aria-hidden', 'true');
    trigger.type = 'button';
    trigger.id = `${select.id || 'select'}-trigger-${++sequence}`;
    trigger.setAttribute('role', 'combobox');
    trigger.setAttribute('aria-haspopup', role);
    trigger.setAttribute('aria-expanded', 'false');
    trigger.append(value, arrow);
    select.before(root);
    root.append(select, trigger);
    select.hidden = true;
    select.tabIndex = -1;
    const panel = element('div', 'select-panel');
    const search = element('input', 'select-search');
    search.type = 'search';
    search.placeholder = '搜索名称或服务…';
    search.setAttribute('role', 'combobox');
    search.setAttribute('aria-autocomplete', 'list');
    search.setAttribute('aria-expanded', 'true');
    search.setAttribute('aria-haspopup', role);
    search.setAttribute('aria-label', `搜索${label}`);
    const list = element('div', 'select-options');
    list.id = `${trigger.id}-options`;
    list.setAttribute('role', role);
    list.setAttribute('aria-label', label);
    trigger.setAttribute('aria-controls', list.id);
    search.setAttribute('aria-controls', list.id);
    const count = element('div', 'select-count');
    panel.append(search, list, count);
    panel.hidden = true;
    const control = { select, root, trigger, value, label, panel, search, list, count, collapsible,
      collapsed: preferences.get(preferenceKey), searchable: false, active: -1, visible: [] };
    controls.set(select, control);
    trigger.addEventListener('click', (event) => { event.preventDefault(); open(control); });
    trigger.addEventListener('keydown', (event) => keydown(control, event));
    search.addEventListener('keydown', (event) => keydown(control, event));
    search.addEventListener('input', () => { renderOptions(control, search.value, null, true); position(control); });
    search.addEventListener('click', (event) => event.stopPropagation());
    select.addEventListener('change', () => sync(control));
    select.addEventListener('invalid', (event) => {
      event.preventDefault();
      trigger.setAttribute('aria-invalid', 'true');
      trigger.focus();
      trigger.title = select.validationMessage;
    });
    control.observer = new MutationObserver(() => sync(control));
    control.observer.observe(select, { subtree: true, childList: true, attributes: true, attributeFilter: ['disabled', 'required', 'selected', 'label'] });
    sync(control);
  }

  /** 页面或对话框刷新后同步控件，释放已移除表单的监听和浮层。 */
  function refresh() {
    for (const [select, control] of controls) {
      if (!select.isConnected) {
        if (opened === control) close(false);
        control.observer.disconnect();
        controls.delete(select);
      }
    }
    document.querySelectorAll('select').forEach((select) => {
      if (!controls.has(select)) enhance(select);
      else sync(controls.get(select));
    });
  }

  /** 点击列表之外关闭；焦点离开时不干扰下一项操作。 */
  function dismiss(event) {
    if (opened && !opened.root.contains(event.target) && !opened.panel.contains(event.target)) close(false);
  }
  document.addEventListener('pointerdown', dismiss);
  document.addEventListener('focusin', dismiss);
  window.addEventListener('resize', () => { if (opened) position(opened); });
  document.addEventListener('scroll', (event) => { if (opened && !opened.panel.contains(event.target)) position(opened); }, true);
  return { refresh, close };
})();
