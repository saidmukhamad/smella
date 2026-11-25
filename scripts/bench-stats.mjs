export function stats(numbers) {
  const sorted = numbers.toSorted((a,b) => a-b);
  const quantile = q => Number(sorted[Math.ceil(q * sorted.length) - 1].toFixed(2));
  return { p50: quantile(.5), p95: quantile(.95) };
}
