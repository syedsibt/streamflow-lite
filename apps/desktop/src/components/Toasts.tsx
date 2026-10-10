import { useStudio } from "../lib/store";

export default function Toasts() {
  const toasts = useStudio((s) => s.toasts);

  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className="toast">
          {t.text}
        </div>
      ))}
    </div>
  );
}
