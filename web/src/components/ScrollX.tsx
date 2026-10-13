import { type ReactNode, useEffect, useRef, useState } from "react";

const FADE = "1.5rem";

/**
 * Scrolls sideways when its content is too wide, and fades the edge that has
 * more to see. Touch screens hide scrollbars, so the fade is the only sign.
 */
export function ScrollX({ children, className = "" }: { children: ReactNode; className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [more, setMore] = useState({ left: false, right: false });

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const update = () =>
      setMore({
        left: el.scrollLeft > 1,
        right: el.scrollLeft + el.clientWidth < el.scrollWidth - 1,
      });
    update();
    el.addEventListener("scroll", update, { passive: true });
    const observer = new ResizeObserver(update);
    observer.observe(el);
    if (el.firstElementChild) observer.observe(el.firstElementChild);
    return () => {
      el.removeEventListener("scroll", update);
      observer.disconnect();
    };
  }, []);

  const mask =
    more.left || more.right
      ? `linear-gradient(to right, ${more.left ? "transparent" : "#000"}, #000 ${FADE}, #000 calc(100% - ${FADE}), ${more.right ? "transparent" : "#000"})`
      : undefined;
  return (
    <div ref={ref} className={`overflow-x-auto ${className}`} style={{ maskImage: mask }}>
      {children}
    </div>
  );
}
