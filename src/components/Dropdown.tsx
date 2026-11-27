import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import { Check, ChevronDown } from 'lucide-react';
import { isTopEscapeLayer, popEscapeLayer, pushEscapeLayer } from '../lib/escapeStack';
import { useSyncStore } from '../store/useSyncStore';

type Option<T> = readonly [value: T, label: string];

export function Dropdown<T extends string | number>({
  label,
  value,
  options,
  onChange,
  placeholder = 'Select…',
  className = '',
  disabled = false,
}: {
  label: string;
  value: T;
  options: ReadonlyArray<Option<T>>;
  onChange: (value: T) => void;
  placeholder?: string;
  className?: string;
  disabled?: boolean;
}) {
  const id = useId();
  const activeNav = useSyncStore((s) => s.activeNav);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLUListElement>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [position, setPosition] = useState<CSSProperties>({});
  const selected = options.findIndex(([key]) => key === value);

  // Close any open portaled menu when switching tabs so hidden views never keep portals visible
  // or swallow Escape key events.
  useEffect(() => {
    setOpen(false);
  }, [activeNav]);

  const choose = (index: number) => {
    const option = options[index];
    if (option && option[0] !== value) onChange(option[0]);
    setOpen(false);
    trigger.current?.focus();
  };

  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const rect = trigger.current?.getBoundingClientRect();
      if (!rect) return;
      const inset = 8;
      const gap = 5;
      const below = window.innerHeight - rect.bottom - inset - gap;
      const above = rect.top - inset - gap;
      const upwards = below < Math.min(options.length * 34 + 12, 240) && above > below;
      const width = Math.min(window.innerWidth - 2 * inset, Math.max(150, rect.width));
      setPosition({
        left: Math.max(inset, Math.min(rect.left, window.innerWidth - width - inset)),
        width,
        maxHeight: Math.max(0, Math.min(256, upwards ? above : below)),
        ...(upwards
          ? { bottom: window.innerHeight - rect.top + gap }
          : { top: rect.bottom + gap }),
      });
    };
    place();
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => {
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
    };
  }, [open, options.length]);

  useEffect(() => {
    if (!open) return;
    const layer = pushEscapeLayer();
    const dismiss = (event: Event) => {
      if (
        event.target instanceof Node &&
        !trigger.current?.contains(event.target) &&
        !menu.current?.contains(event.target)
      ) {
        setOpen(false);
      }
    };
    const onDocKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape' && isTopEscapeLayer(layer)) {
        event.preventDefault();
        event.stopPropagation();
        setOpen(false);
        trigger.current?.focus();
      }
    };
    document.addEventListener('pointerdown', dismiss);
    document.addEventListener('keydown', onDocKeyDown);
    return () => {
      document.removeEventListener('pointerdown', dismiss);
      document.removeEventListener('keydown', onDocKeyDown);
      popEscapeLayer(layer);
    };
  }, [open]);

  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    const last = options.length - 1;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const step = event.key === 'ArrowDown' ? 1 : -1;
      setActive(open ? Math.min(last, Math.max(0, active + step)) : Math.max(0, selected));
      setOpen(true);
    } else if (open && (event.key === 'Enter' || event.key === ' ')) {
      event.preventDefault();
      choose(active);
    }
  };

  return (
    <div className={`dropdown ${className}`}>
      <button
        ref={trigger}
        type="button"
        className="button dropdown-trigger"
        role="combobox"
        aria-label={label}
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        disabled={disabled || options.length === 0}
        onClick={() => {
          setActive(Math.max(0, selected));
          setOpen(!open);
        }}
        onKeyDown={onKeyDown}
      >
        <span>{options[selected]?.[1] ?? placeholder}</span>
        <ChevronDown size={14} aria-hidden="true" />
      </button>
      {open &&
        createPortal(
          <ul ref={menu} id={id} role="listbox" aria-label={label} className="dropdown-menu" style={position}>
            {options.map(([key, text], index) => (
              <li
                key={String(key)}
                role="option"
                aria-selected={key === value}
                className={index === active ? 'is-highlighted' : undefined}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => choose(index)}
              >
                <span>{text}</span>
                {key === value && <Check size={14} aria-hidden="true" />}
              </li>
            ))}
          </ul>,
          document.body,
        )}
    </div>
  );
}
