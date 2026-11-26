const layers: symbol[] = [];

export const pushEscapeLayer = (): symbol => {
  const token = Symbol('escape-layer');
  layers.push(token);
  return token;
};

export const popEscapeLayer = (token: symbol): void => {
  const index = layers.indexOf(token);
  if (index >= 0) layers.splice(index, 1);
};

export const isTopEscapeLayer = (token: symbol): boolean =>
  layers[layers.length - 1] === token;
